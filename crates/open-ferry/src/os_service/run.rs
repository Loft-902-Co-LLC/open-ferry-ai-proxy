//! `open-ferry service run`, which the scheduled task and the Windows
//! service start; nothing else should.
//!
//! - For the task, `service run -config <path>` lets go of the console
//!   window Windows opened for it, which closes it, and runs the server,
//!   `open-ferry -config <path>`, as a child with no window, its output
//!   going to `service.log` in the config's directory. When the server
//!   fails it is started again after 5 seconds, or after twice the last wait
//!   when it failed within a minute of starting, up to a minute; when it
//!   exits cleanly, so does `service run`. The server is in a job that ends
//!   it when `service run` ends, however that ends: when the task is
//!   stopped, or the user logs off.
//! - For the Windows service, `service run -system -config <path>` answers
//!   the service manager and serves in the same process, its output going
//!   to the same `service.log`, until the service manager stops it. A
//!   server that exits with an error is reported to the service manager as
//!   failed, and the service manager starts it again.
//!
//! Both run in the config's directory, as the systemd unit and the launchd
//! job do, or in the directory `-dir` names (`migrate` gives it the
//! directory CLIProxyAPI ran in), so the server reads the `.env` file there,
//! and `service.log` is kept there under 10 MiB by moving it to
//! `service.log.1` as the server starts.

use std::ffi::OsString;
use std::fmt::Display;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::windows::io::IntoRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::{define_windows_service, service_dispatcher};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Console::{
    FreeConsole, GetConsoleProcessList, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, GetCurrentProcess};

use super::SERVICE_NAME;
use super::windows::LOG_FILE;
use crate::flags::Flags;
use open_ferry_supervisor::timings::STOP_BUDGET;

/// The size past which `service.log` is moved aside as the server starts.
const MAX_LOG: u64 = 10 * 1024 * 1024;

/// The wait before the server is started again after a failure.
const FIRST_DELAY: Duration = Duration::from_secs(5);

/// The longest wait before the server is started again; a server that ran
/// this long before it failed waits [`FIRST_DELAY`] again.
const MAX_DELAY: Duration = Duration::from_secs(60);

/// How long the Windows service tells the service manager stopping may
/// take.
const STOP_WAIT: Duration = Duration::from_secs(30);

/// Runs the server for the task, or with `system` for the Windows service,
/// with the config at the full path `config`, in `dir` or else the config's
/// directory. With `supervised` (the data directory, a full path) the task
/// runs the blue-green supervisor, not the server, and fails closed: unless
/// the job that ends it with this process is in place, nothing is started.
pub(super) fn main(
    config: &str,
    dir: Option<&str>,
    system: bool,
    supervised: Option<&str>,
) -> ExitCode {
    let config = PathBuf::from(config);
    let dir = match (dir, config.parent()) {
        (Some(dir), _) if Path::new(dir).is_absolute() => PathBuf::from(dir),
        (Some(_), _) => {
            eprintln!("service run needs -dir to be a full path");
            return ExitCode::from(2);
        }
        (None, Some(dir)) if config.is_absolute() => dir.to_path_buf(),
        _ => {
            eprintln!("service run needs the config's full path");
            return ExitCode::from(2);
        }
    };
    if !config.is_absolute() {
        eprintln!("service run needs the config's full path");
        return ExitCode::from(2);
    }
    if let Err(error) = std::env::set_current_dir(&dir) {
        eprintln!("failed to go to {}: {error}", dir.display());
        return ExitCode::FAILURE;
    }
    let log = dir.join(LOG_FILE);
    if system
        && supervised.is_some()
        && let Err(message) = crate::supervised::require_system_account()
    {
        return refuse_for_service_manager(&log, message);
    }
    if system {
        serve_for_service_manager(config, log, supervised.map(PathBuf::from))
    } else {
        supervise(&config, &log, supervised.map(Path::new))
    }
}

/// The child's arguments: the server's `-config <path>`, or with `data_dir`
/// the supervisor's `-supervise -config <path> -data-dir <path>`.
fn child_arguments(config: &Path, data_dir: Option<&Path>) -> Vec<OsString> {
    let mut arguments = Vec::new();
    if data_dir.is_some() {
        arguments.push(OsString::from("-supervise"));
    }
    arguments.push(OsString::from("-config"));
    arguments.push(config.as_os_str().to_owned());
    if let Some(data_dir) = data_dir {
        arguments.push(OsString::from("-data-dir"));
        arguments.push(data_dir.as_os_str().to_owned());
    }
    arguments
}

/// What to do about `contained`, how this process fared at being put in a job
/// that ends the child with it: nothing when it worked; when it didn't, a
/// plain run says so and goes on (the server may outlive the wrapper), while
/// a supervised one fails closed, because a supervisor and its workers that
/// outlive the wrapper would keep serving with nothing to stop them, and
/// block a restart.
fn containment(supervised: bool, contained: io::Result<()>) -> Result<Option<String>, String> {
    match contained {
        Ok(()) => Ok(None),
        Err(error) if supervised => Err(format!(
            "failed to make a job that ends the supervisor and its workers with this process: {error}; not starting them"
        )),
        Err(error) => Ok(Some(format!(
            "the server may outlive this process, which failed to make a job: {error}"
        ))),
    }
}

/// Runs the server as a child with no window, and again when it fails; with
/// `data_dir`, the supervisor.
fn supervise(config: &Path, log: &Path, data_dir: Option<&Path>) -> ExitCode {
    detach_console();
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            note(
                log,
                format_args!("failed to find the open-ferry binary: {error}"),
            );
            return ExitCode::FAILURE;
        }
    };
    match containment(data_dir.is_some(), end_children_with_this_process()) {
        Ok(None) => {}
        Ok(Some(warning)) => note(log, warning),
        Err(message) => {
            note(log, message);
            return ExitCode::FAILURE;
        }
    }
    let arguments = child_arguments(config, data_dir);
    let mut delay = None;
    loop {
        rotate(log);
        let started = Instant::now();
        let status = open_log(log).and_then(|output| {
            let errors = output.try_clone()?;
            Command::new(&exe)
                .args(&arguments)
                .stdin(Stdio::null())
                .stdout(output)
                .stderr(errors)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()?
                .wait()
        });
        let wait = restart_delay(delay, started.elapsed());
        let seconds = wait.as_secs();
        match status {
            Ok(status) if status.success() => {
                note(log, "the server exited cleanly, so it isn't started again");
                return ExitCode::SUCCESS;
            }
            Ok(status) => match status.code() {
                Some(code) => note(
                    log,
                    format_args!(
                        "the server exited with code {code}; starting it again in {seconds} s"
                    ),
                ),
                None => note(
                    log,
                    format_args!("the server exited; starting it again in {seconds} s"),
                ),
            },
            Err(error) => note(
                log,
                format_args!("failed to start the server: {error}; trying again in {seconds} s"),
            ),
        }
        delay = Some(wait);
        std::thread::sleep(wait);
    }
}

/// How long to wait before starting the server again, after the `previous`
/// wait, when it ran for `ran_for` before it failed.
fn restart_delay(previous: Option<Duration>, ran_for: Duration) -> Duration {
    match previous {
        Some(previous) if ran_for < MAX_DELAY => (previous * 2).min(MAX_DELAY),
        _ => FIRST_DELAY,
    }
}

/// Closes the console window Windows opened for the task, unless another
/// process shares it, as when `service run` is started from a terminal.
fn detach_console() {
    let mut processes = [0u32; 2];
    // SAFETY: `processes` has room for the 2 IDs the call is told of.
    let count = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), 2) };
    if count == 1 {
        // SAFETY: nothing here writes to the console after this: what is
        // said goes to the log.
        unsafe { FreeConsole() };
    }
}

/// Puts this process in a job that ends every process in it when this one
/// ends, so the server, which is started in it, doesn't outlive it.
fn end_children_with_this_process() -> io::Result<()> {
    contain(|job| {
        // SAFETY: both handles are valid.
        if unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })
}

/// Makes a job that ends its processes when its handle closes, and has
/// `assign` put this process in it. On success the handle is never closed, so
/// it closes, and the job ends its processes, when this process ends (closing
/// it earlier, while this process is in the job, would end this process). On a
/// failure it is closed here, when this process is not in the job, so nothing
/// is left open.
fn contain(assign: impl FnOnce(HANDLE) -> io::Result<()>) -> io::Result<()> {
    // SAFETY: no attributes and no name ask for an unnamed job with the
    // default security.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let made = set_kill_on_close(job).and_then(|()| assign(job));
    if made.is_err() {
        // SAFETY: `job` is a valid handle of ours, and this process is not in
        // the job: the assignment failed or never ran.
        unsafe { CloseHandle(job) };
    }
    made
}

/// Has `job` end its processes when its last handle closes.
fn set_kill_on_close(job: HANDLE) -> io::Result<()> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let size = u32::try_from(size_of_val(&limits)).map_err(io::Error::other)?;
    // SAFETY: `limits` is the structure the class names, and `size` is its
    // size.
    let set = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size,
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_log(log: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(log)
}

/// Moves `log` to `<log>.1` when it has grown past [`MAX_LOG`].
fn rotate(log: &Path) {
    if fs::metadata(log).is_ok_and(|metadata| metadata.len() > MAX_LOG) {
        let mut old = log.as_os_str().to_owned();
        old.push(".1");
        let _ = fs::rename(log, old);
    }
}

/// Adds a line of `service run`'s own to `log`.
fn note(log: &Path, line: impl Display) {
    if let Ok(mut file) = open_log(log) {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(file, "{now} open-ferry service run: {line}");
    }
}

define_windows_service!(ffi_service_main, service_main);

/// What `service_main` serves with: what `serve_for_service_manager` found
/// before the service manager started its thread.
static PREPARED: Mutex<Option<Prepared>> = Mutex::new(None);

/// How the server exited, for `serve_for_service_manager` to return.
static EXITED: Mutex<Option<ExitCode>> = Mutex::new(None);

/// A refused SCM entry still connects to the dispatcher so it can report
/// Stopped with a service-specific failure. A hand run prints the same refusal.
static REFUSED: Mutex<Option<String>> = Mutex::new(None);

fn refuse_for_service_manager(log: &Path, message: String) -> ExitCode {
    note(log, &message);
    if let Ok(mut refused) = REFUSED.lock() {
        *refused = Some(message.clone());
    }
    if service_dispatcher::start(SERVICE_NAME, ffi_service_main).is_err() {
        eprintln!("{message}");
    }
    ExitCode::FAILURE
}

struct Prepared {
    config: PathBuf,
    log: PathBuf,
    working_dir: io::Result<PathBuf>,
    dotenv: Option<Result<(), crate::dotenv::Error>>,
    supervised: Option<PathBuf>,
}

/// Hands the process to the service manager, which runs `service_main` on
/// a thread of its own, and returns once the service has stopped.
fn serve_for_service_manager(
    config: PathBuf,
    log: PathBuf,
    supervised: Option<PathBuf>,
) -> ExitCode {
    // As `main` does, before any thread starts.
    let working_dir = std::env::current_dir();
    let dotenv = working_dir
        .as_ref()
        .ok()
        .map(|dir| crate::load_dotenv(&dir.join(".env")));
    if let Some(Err(error)) = &dotenv
        && error.is_refused()
    {
        return refuse_for_service_manager(&log, error.to_string());
    }
    if let Err(error) = check_config(&crate::scope::Host, &config) {
        return refuse_for_service_manager(&log, error);
    }
    if let Ok(mut prepared) = PREPARED.lock() {
        *prepared = Some(Prepared {
            config,
            log,
            working_dir,
            dotenv,
            supervised,
        });
    }
    if let Err(error) = service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        eprintln!(
            "service run -system is for the Windows service manager, which starts it as the service: {error}"
        );
        return ExitCode::FAILURE;
    }
    EXITED
        .lock()
        .ok()
        .and_then(|mut exited| exited.take())
        .unwrap_or(ExitCode::FAILURE)
}

/// Checks the service's settings and resolved auth directory before dispatch.
fn check_config(probe: &dyn crate::scope::Probe, path: &Path) -> Result<(), String> {
    let config = crate::settings::load(probe, path)?;
    let auth_dir = config
        .resolve_auth_dir()
        .map_err(|error| format!("failed to resolve auth directory: {error}"))?;
    crate::scope::authorize_auth_dir(probe, &auth_dir).map_err(|error| error.to_string())
}

fn service_main(_arguments: Vec<OsString>) {
    let refusal = REFUSED.lock().ok().and_then(|mut refused| refused.take());
    let code = if let Some(message) = refusal {
        eprintln!("{message}");
        if let Ok(status) = service_control_handler::register(SERVICE_NAME, |control| {
            if control == ServiceControl::Interrogate {
                ServiceControlHandlerResult::NoError
            } else {
                ServiceControlHandlerResult::NotImplemented
            }
        }) {
            report(
                status,
                ServiceState::Stopped,
                ServiceExitCode::ServiceSpecific(1),
            );
        }
        ExitCode::FAILURE
    } else {
        serve_as_service()
    };
    if let Ok(mut exited) = EXITED.lock() {
        *exited = Some(code);
    }
}

/// Serves until the service manager stops the service, telling it how the
/// service is.
fn serve_as_service() -> ExitCode {
    let Some(prepared) = PREPARED
        .lock()
        .ok()
        .and_then(|mut prepared| prepared.take())
    else {
        return ExitCode::FAILURE;
    };
    if prepared.supervised.is_some() {
        return supervise_as_service(prepared);
    }
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let mut stop = Some(stop);
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(stop) = stop.take() {
                let _ = stop.send(());
            }
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let Ok(status) = service_control_handler::register(SERVICE_NAME, handler) else {
        return ExitCode::FAILURE;
    };
    rotate(&prepared.log);
    // Without the log, the output goes nowhere; the service still serves.
    let _ = redirect_output(&prepared.log);
    report(status, ServiceState::Running, ServiceExitCode::Win32(0));
    let flags = Flags {
        config: prepared.config.to_string_lossy().into_owned(),
        ..Flags::default()
    };
    let stop = async move {
        let _ = stopped.await;
        report(status, ServiceState::StopPending, ServiceExitCode::Win32(0));
    };
    let code = crate::serve(flags, prepared.working_dir, prepared.dotenv, stop);
    let exit_code = if code == ExitCode::SUCCESS {
        ServiceExitCode::Win32(0)
    } else {
        ServiceExitCode::ServiceSpecific(1)
    };
    report(status, ServiceState::Stopped, exit_code);
    code
}

const SUPERVISED_STOP_WAIT: Duration = STOP_BUDGET.saturating_add(Duration::from_secs(10));
const CHECKPOINT_EVERY: Duration = Duration::from_secs(5);

/// Events of the control loop, including a stop received during startup.
enum StatusEvent {
    Started,
    Stop(Duration),
    Tick(Duration),
    Exited(bool),
}

#[derive(Default)]
struct StatusSequence {
    stopping: Option<Duration>,
    checkpoint: u32,
    exited: bool,
}

impl StatusSequence {
    /// Pure status policy; the caller supplies time and reports the result.
    fn event(&mut self, event: StatusEvent) -> Option<ServiceStatus> {
        if self.exited {
            return None;
        }
        let mut elapsed = Duration::ZERO;
        let (state, exit_code, checkpoint) = match event {
            StatusEvent::Started if self.stopping.is_none() => {
                (ServiceState::Running, ServiceExitCode::Win32(0), 0)
            }
            StatusEvent::Stop(now) if self.stopping.is_none() => {
                self.stopping = Some(now);
                self.checkpoint = 1;
                (ServiceState::StopPending, ServiceExitCode::Win32(0), 1)
            }
            StatusEvent::Tick(now) => {
                elapsed = now.checked_sub(self.stopping?)?;
                if elapsed >= SUPERVISED_STOP_WAIT {
                    return None;
                }
                let checkpoint =
                    u32::try_from(elapsed.as_secs() / CHECKPOINT_EVERY.as_secs()).ok()? + 1;
                if checkpoint <= self.checkpoint {
                    return None;
                }
                self.checkpoint = checkpoint;
                (
                    ServiceState::StopPending,
                    ServiceExitCode::Win32(0),
                    checkpoint,
                )
            }
            StatusEvent::Exited(clean) => {
                self.exited = true;
                (
                    ServiceState::Stopped,
                    if clean {
                        ServiceExitCode::Win32(0)
                    } else {
                        ServiceExitCode::ServiceSpecific(1)
                    },
                    0,
                )
            }
            _ => return None,
        };
        Some(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code,
            checkpoint,
            wait_hint: if state == ServiceState::StopPending {
                SUPERVISED_STOP_WAIT
                    .saturating_sub(elapsed)
                    .max(CHECKPOINT_EVERY * 2)
            } else {
                Duration::ZERO
            },
            process_id: None,
        })
    }
}

fn stop_control(control: ServiceControl) -> Option<&'static str> {
    match control {
        ServiceControl::Stop => Some("service stop"),
        ServiceControl::Shutdown => Some("system shutdown"),
        _ => None,
    }
}

/// Queue StopPending before the command can make a fast startup stop exit.
fn queue_service_stop(
    events: &std::sync::mpsc::Sender<StatusEvent>,
    stops: &tokio::sync::mpsc::UnboundedSender<&'static str>,
    name: &'static str,
    now: Duration,
) {
    let _ = events.send(StatusEvent::Stop(now));
    let _ = stops.send(name);
}

/// Runs the same supervisor as `-supervise`, with SCM controls as its stops.
fn supervise_as_service(prepared: Prepared) -> ExitCode {
    let (stops, stopped) = tokio::sync::mpsc::unbounded_channel();
    let (events, received) = std::sync::mpsc::channel();
    let started = Instant::now();
    let controls = events.clone();
    let mut stopping = false;
    let handler = move |control| {
        if let Some(name) = stop_control(control) {
            if !stopping {
                stopping = true;
                queue_service_stop(&controls, &stops, name, started.elapsed());
            }
            ServiceControlHandlerResult::NoError
        } else if control == ServiceControl::Interrogate {
            ServiceControlHandlerResult::NoError
        } else {
            ServiceControlHandlerResult::NotImplemented
        }
    };
    let Ok(status) = service_control_handler::register(SERVICE_NAME, handler) else {
        return ExitCode::FAILURE;
    };
    let mut sequence = StatusSequence::default();
    rotate(&prepared.log);
    let _ = redirect_output(&prepared.log);
    // Defence in depth: the entry point checked the account before dispatch.
    if let Err(message) = crate::supervised::require_system_account() {
        note(&prepared.log, &message);
        eprintln!("{message}");
        if let Some(report) = sequence.event(StatusEvent::Exited(false)) {
            let _ = status.set_service_status(report);
        }
        return ExitCode::FAILURE;
    }
    if let Some(report) = sequence.event(StatusEvent::Started) {
        let _ = status.set_service_status(report);
    }
    let reporter = std::thread::spawn(move || {
        loop {
            // Wake on controls immediately and on each checkpoint boundary.
            let wait = sequence.stopping.map_or(CHECKPOINT_EVERY, |stop| {
                let elapsed = started.elapsed().saturating_sub(stop);
                CHECKPOINT_EVERY
                    - Duration::from_nanos(
                        (elapsed.as_nanos() % CHECKPOINT_EVERY.as_nanos()) as u64,
                    )
            });
            let event = match received.recv_timeout(wait) {
                Ok(event) => event,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    StatusEvent::Tick(started.elapsed())
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => StatusEvent::Exited(false),
            };
            if let Some(report) = sequence.event(event) {
                let _ = status.set_service_status(report);
            }
            if sequence.exited {
                break;
            }
        }
    });
    let flags = Flags {
        config: prepared.config.to_string_lossy().into_owned(),
        system: true,
        supervise: true,
        data_dir: prepared
            .supervised
            .expect("serve_as_service selects this path only with a supervisor data directory")
            .to_string_lossy()
            .into_owned(),
        ..Flags::default()
    };
    let code = crate::supervised::supervise_service(&flags, prepared.dotenv, stopped);
    let _ = events.send(StatusEvent::Exited(code == ExitCode::SUCCESS));
    let _ = reporter.join();
    code
}

/// Sends standard output and standard error to `log`.
fn redirect_output(log: &Path) -> io::Result<()> {
    let handle = open_log(log)?.into_raw_handle();
    for std_handle in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `handle` is an open file's, and is never closed, so it
        // stays valid for the life of the process.
        if unsafe { SetStdHandle(std_handle, handle) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Tells the service manager the service is in `state`.
fn report(status: ServiceStatusHandle, state: ServiceState, exit_code: ServiceExitCode) {
    let (controls_accepted, wait_hint) = match state {
        ServiceState::Running => (
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            Duration::ZERO,
        ),
        ServiceState::StopPending => (ServiceControlAccept::empty(), STOP_WAIT),
        _ => (ServiceControlAccept::empty(), Duration::ZERO),
    };
    let _ = status.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted,
        exit_code,
        checkpoint: 0,
        wait_hint,
        process_id: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_settings_are_checked_before_dispatch_and_auth_paths_before_use() {
        use crate::os_service::Platform;
        use crate::update::scope_probe::Fake;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let mut probe = Fake::new(Platform::Windows, true);
        assert!(
            check_config(&probe, &path)
                .unwrap_err()
                .contains("failed to read")
        );
        probe
            .file(path.to_str().unwrap())
            .untrust(path.to_str().unwrap());
        assert!(
            check_config(&probe, &path)
                .unwrap_err()
                .contains("Refusing to load")
        );
        probe.untrusted.clear();
        fs::write(&path, "auth-dir: C:/ProgramData/open-ferry/versions\n").unwrap();
        assert!(
            check_config(&probe, &path)
                .unwrap_err()
                .contains("machine's records")
        );
        fs::write(&path, "auth-dir: C:/ProgramData/open-ferry/auth\n").unwrap();
        assert!(check_config(&probe, &path).is_ok());
        probe.privileged = Ok(false);
        fs::write(&path, "auth-dir: C:/ProgramData/open-ferry\n").unwrap();
        assert!(check_config(&probe, &path).is_ok());
    }

    #[derive(Default)]
    struct Reporter {
        sequence: StatusSequence,
        reports: Vec<ServiceStatus>,
    }

    impl Reporter {
        fn event(&mut self, event: StatusEvent) {
            if let Some(status) = self.sequence.event(event) {
                self.reports.push(status);
            }
        }
    }

    #[test]
    fn a_stop_status_is_queued_before_the_supervisor_can_exit() {
        for control in [ServiceControl::Stop, ServiceControl::Shutdown] {
            let (events, received) = std::sync::mpsc::channel();
            let (stops, mut stopped) = tokio::sync::mpsc::unbounded_channel();
            let exited = events.clone();
            let worker = std::thread::spawn(move || {
                assert!(stopped.blocking_recv().is_some());
                exited.send(StatusEvent::Exited(true)).unwrap();
            });
            queue_service_stop(
                &events,
                &stops,
                stop_control(control).unwrap(),
                Duration::ZERO,
            );
            worker.join().unwrap();
            let mut reporter = Reporter::default();
            reporter.event(StatusEvent::Started);
            reporter.event(received.try_recv().unwrap());
            reporter.event(received.try_recv().unwrap());
            assert_eq!(
                reporter
                    .reports
                    .iter()
                    .map(|status| status.current_state)
                    .collect::<Vec<_>>(),
                [
                    ServiceState::Running,
                    ServiceState::StopPending,
                    ServiceState::Stopped
                ]
            );
        }
    }

    #[test]
    fn supervised_stop_reports_increasing_checkpoints_only_before_the_deadline() {
        let mut reporter = Reporter::default();
        reporter.event(StatusEvent::Started);
        reporter.event(StatusEvent::Tick(Duration::ZERO));
        reporter.event(StatusEvent::Stop(Duration::from_secs(20)));
        reporter.event(StatusEvent::Stop(Duration::from_secs(21)));
        for seconds in 20..=85 {
            reporter.event(StatusEvent::Tick(Duration::from_secs(seconds)));
        }
        assert_eq!(reporter.reports[0].current_state, ServiceState::Running);
        assert_eq!(reporter.reports[0].wait_hint, Duration::ZERO);
        assert_eq!(
            reporter.reports[0].controls_accepted,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        );
        let pending = &reporter.reports[1..];
        assert_eq!(pending.len(), 12);
        for (index, status) in pending.iter().enumerate() {
            assert_eq!(status.current_state, ServiceState::StopPending);
            assert_eq!(status.checkpoint, u32::try_from(index).unwrap() + 1);
            assert_eq!(
                status.wait_hint,
                Duration::from_secs((60 - index as u64 * 5).max(10))
            );
            assert!(status.controls_accepted.is_empty());
        }
        reporter.event(StatusEvent::Exited(true));
        let stopped = reporter.reports.last().unwrap();
        assert_eq!(stopped.current_state, ServiceState::Stopped);
        assert_eq!(stopped.exit_code, ServiceExitCode::Win32(0));
        assert_eq!(stopped.checkpoint, 0);
        assert_eq!(stopped.wait_hint, Duration::ZERO);
        reporter.event(StatusEvent::Tick(Duration::from_secs(90)));
        reporter.event(StatusEvent::Stop(Duration::from_secs(90)));
        assert_eq!(reporter.reports.len(), 14);
    }

    #[test]
    fn an_early_stop_and_a_startup_failure_have_the_same_exit_mapping() {
        for clean in [true, false] {
            let mut reporter = Reporter::default();
            reporter.event(StatusEvent::Started);
            // Startup is still waiting for its locks; no ready worker is needed.
            reporter.event(StatusEvent::Stop(Duration::ZERO));
            reporter.event(StatusEvent::Exited(clean));
            assert_eq!(
                reporter
                    .reports
                    .iter()
                    .map(|s| s.current_state)
                    .collect::<Vec<_>>(),
                [
                    ServiceState::Running,
                    ServiceState::StopPending,
                    ServiceState::Stopped
                ]
            );
            assert_eq!(
                reporter.reports[2].exit_code,
                if clean {
                    ServiceExitCode::Win32(0)
                } else {
                    ServiceExitCode::ServiceSpecific(1)
                }
            );
        }
        let mut reporter = Reporter::default();
        reporter.event(StatusEvent::Exited(false));
        assert_eq!(
            reporter.reports[0].exit_code,
            ServiceExitCode::ServiceSpecific(1)
        );
        assert_eq!(reporter.reports[0].current_state, ServiceState::Stopped);
    }

    #[tokio::test]
    async fn shutdown_and_stop_feed_the_same_early_command_path() {
        for control in [ServiceControl::Stop, ServiceControl::Shutdown] {
            let (signals, stopped) = tokio::sync::mpsc::unbounded_channel();
            let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
            signals.send(stop_control(control).unwrap()).unwrap();
            drop(signals);
            crate::supervised::forward_stops(stopped, commands).await;
            assert!(matches!(
                received.try_recv(),
                Ok(open_ferry_supervisor::core::Command::Stop)
            ));
        }
        assert!(stop_control(ServiceControl::Interrogate).is_none());
    }

    #[tokio::test]
    async fn scm_controls_stop_the_supervisor_during_its_update_lock_wait() {
        use crate::supervised::{Setup, SupervisedHost, startup_config};
        use open_ferry_supervisor::core::Exit;
        use open_ferry_supervisor::driver::ProcessDriver;
        use open_ferry_supervisor::endpoint::ServiceKind;
        use open_ferry_update::DataDir;

        for control in [ServiceControl::Stop, ServiceControl::Shutdown] {
            let root = tempfile::tempdir().unwrap();
            let data = DataDir::at(root.path().join("data"));
            let _lock = data.try_lock().unwrap().unwrap();
            let installed = root.path().join("not-a-worker");
            fs::write(&installed, b"never started").unwrap();
            let config = root.path().join("config.yaml");
            let (signals, stopped) = tokio::sync::mpsc::unbounded_channel();
            let (commands, received) = tokio::sync::mpsc::unbounded_channel();
            let forward = tokio::spawn(crate::supervised::forward_stops(stopped, commands.clone()));
            let name = stop_control(control).unwrap();
            let waited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let observed = waited.clone();
            let host = SupervisedHost::new(config.clone(), installed.clone(), Vec::new())
                .with_waiting_hook(move |_| {
                    observed.store(true, std::sync::atomic::Ordering::SeqCst);
                    signals.send(name).unwrap();
                });
            let cfg = startup_config(Setup {
                config,
                kind: ServiceKind::User,
                data,
                installed,
                #[cfg(feature = "blue-green-test")]
                endpoint: None,
            });
            let driver = ProcessDriver::new(cfg.kind, cfg.timings.exit_poll);
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                open_ferry_supervisor::supervise::supervise(
                    &cfg,
                    &host,
                    &driver,
                    &open_ferry_supervisor::clock::SystemClock,
                    commands,
                    received,
                    open_ferry_supervisor::supervise::default_limit(),
                ),
            )
            .await
            .unwrap();
            assert!(matches!(result, Ok(Exit::Stopped)), "{result:?}");
            assert!(waited.load(std::sync::atomic::Ordering::SeqCst));
            assert!(!cfg.data.root().join("supervisor.json").exists());
            let mut reporter = Reporter::default();
            reporter.event(StatusEvent::Started);
            reporter.event(StatusEvent::Stop(Duration::ZERO));
            reporter.event(StatusEvent::Exited(
                crate::supervised::exit_status(&result) == 0,
            ));
            assert_eq!(
                reporter.reports.last().unwrap().exit_code,
                ServiceExitCode::Win32(0)
            );
            forward.abort();
        }
    }

    #[test]
    fn restart_waits_longer_while_the_server_keeps_failing() {
        // Not upstream's: the task's restart policy.
        let soon = Duration::from_secs(1);
        assert_eq!(restart_delay(None, soon), FIRST_DELAY);
        assert_eq!(
            restart_delay(Some(FIRST_DELAY), soon),
            Duration::from_secs(10)
        );
        assert_eq!(
            restart_delay(Some(Duration::from_secs(40)), soon),
            MAX_DELAY
        );
        assert_eq!(restart_delay(Some(MAX_DELAY), soon), MAX_DELAY);
        // A server that ran for a while starts the waits over.
        assert_eq!(restart_delay(Some(MAX_DELAY), MAX_DELAY), FIRST_DELAY);
    }

    #[test]
    fn a_failed_assignment_fails_a_supervised_run_closed_and_a_plain_one_goes_on() {
        // Not upstream's: the supervised task fails closed.
        let failed = || contain(|_| Err(io::Error::other("injected")));
        // The injected failure reaches the policy, and this process, which
        // was never in the job, is still here to read it.
        let error = failed().unwrap_err();
        assert_eq!(error.to_string(), "injected");
        let message = containment(true, failed()).unwrap_err();
        assert!(message.contains("injected"), "{message}");
        assert!(message.contains("not starting"), "{message}");
        let warning = containment(false, failed()).unwrap().unwrap();
        assert!(warning.contains("may outlive"), "{warning}");
        assert_eq!(containment(true, Ok(())), Ok(None));
        assert_eq!(containment(false, Ok(())), Ok(None));
    }

    #[test]
    fn the_child_is_the_server_or_the_supervisor() {
        let config = Path::new(r"C:\ferry\config.yaml");
        let data = Path::new(r"C:\ferry\data");
        assert_eq!(
            child_arguments(config, None),
            ["-config", r"C:\ferry\config.yaml"]
        );
        assert_eq!(
            child_arguments(config, Some(data)),
            [
                "-supervise",
                "-config",
                r"C:\ferry\config.yaml",
                "-data-dir",
                r"C:\ferry\data"
            ]
        );
    }

    #[test]
    fn rotates_only_a_big_log() {
        // Not upstream's: `service.log` is moved aside past 10 MiB.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(LOG_FILE);
        fs::write(&log, b"small").unwrap();
        rotate(&log);
        assert!(log.exists());
        let file = File::options().write(true).open(&log).unwrap();
        file.set_len(MAX_LOG + 1).unwrap();
        drop(file);
        rotate(&log);
        assert!(!log.exists());
        assert_eq!(
            fs::metadata(dir.path().join("service.log.1"))
                .unwrap()
                .len(),
            MAX_LOG + 1
        );
    }

    #[test]
    fn an_scm_refusal_is_written_to_the_service_log_before_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(LOG_FILE);
        note(&log, "a system supervisor runs only as the system service");
        assert!(
            fs::read_to_string(log)
                .unwrap()
                .contains("a system supervisor runs only as the system service")
        );
    }
}
