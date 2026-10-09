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
/// directory.
pub(super) fn main(config: &str, dir: Option<&str>, system: bool) -> ExitCode {
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
    if system {
        serve_for_service_manager(config, log)
    } else {
        supervise(&config, &log)
    }
}

/// Runs the server as a child with no window, and again when it fails.
fn supervise(config: &Path, log: &Path) -> ExitCode {
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
    if let Err(error) = end_children_with_this_process() {
        note(
            log,
            format_args!(
                "the server may outlive this process, which failed to make a job: {error}"
            ),
        );
    }
    let mut delay = None;
    loop {
        rotate(log);
        let started = Instant::now();
        let status = open_log(log).and_then(|output| {
            let errors = output.try_clone()?;
            Command::new(&exe)
                .arg("-config")
                .arg(config)
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
    // SAFETY: no attributes and no name ask for an unnamed job with the
    // default security.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
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
    // SAFETY: both handles are valid. The job's is never closed, so it
    // closes, and the job ends its processes, when this process ends.
    if unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == 0 {
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

struct Prepared {
    config: PathBuf,
    log: PathBuf,
    working_dir: io::Result<PathBuf>,
    dotenv: Option<Result<(), crate::dotenv::Error>>,
}

/// Hands the process to the service manager, which runs `service_main` on
/// a thread of its own, and returns once the service has stopped.
fn serve_for_service_manager(config: PathBuf, log: PathBuf) -> ExitCode {
    // As `main` does, before any thread starts.
    let working_dir = std::env::current_dir();
    let dotenv = working_dir
        .as_ref()
        .ok()
        .map(|dir| crate::load_dotenv(&dir.join(".env")));
    if let Ok(mut prepared) = PREPARED.lock() {
        *prepared = Some(Prepared {
            config,
            log,
            working_dir,
            dotenv,
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

fn service_main(_arguments: Vec<OsString>) {
    let code = serve_as_service();
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
}
