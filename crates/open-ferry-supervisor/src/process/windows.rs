//! Windows process runner: spawn a worker inside a kill-on-close job with only
//! its standard handles inherited, wait for it and end it by handle, and check
//! whether a recorded process is still the one that was recorded.
//!
//! A process is named by its id and its creation time (`FILETIME`, 100 ns
//! ticks since 1601). Ids are reused; the pair is not.

use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io;
use std::mem::{size_of, size_of_val, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use windows_sys::Win32::Foundation::{
    DuplicateHandle, FILETIME, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetCurrentProcessId, GetExitCodeProcess,
    GetProcessTimes, INFINITE, InitializeProcThreadAttributeList, OpenProcess,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, RegisterWaitForSingleObject,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UnregisterWaitEx,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::ProcessIdentity;

const SYNCHRONIZE: u32 = 0x0010_0000;
const PROCESS_TERMINATE: u32 = 0x0001;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
const DUPLICATE_SAME_ACCESS: u32 = 0x0002;
const WT_EXECUTEONLYONCE: u32 = 0x0008;
const ERROR_INVALID_PARAMETER: i32 = 87;
/// The exit code the runner gives a process it ends.
const KILLED: u32 = 1;

/// Handles are inheritable only for the moment between making them and
/// `CreateProcessW`; this keeps two spawns of ours from overlapping in it.
static SPAWN: Mutex<()> = Mutex::new(());

fn os_error() -> io::Error {
    io::Error::last_os_error()
}

fn creation_time(process: HANDLE) -> io::Result<u64> {
    // SAFETY: FILETIME is plain data that the call fills in.
    let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
        unsafe { (zeroed(), zeroed(), zeroed(), zeroed()) };
    // SAFETY: all four out pointers are valid; a bad handle only fails the call.
    let ok = unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) };
    if ok == 0 {
        return Err(os_error());
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// Whether the handle is signalled within `millis`.
fn wait_raw(handle: HANDLE, millis: u32) -> io::Result<bool> {
    // SAFETY: a bad handle only fails the call.
    match unsafe { WaitForSingleObject(handle, millis) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(os_error()),
    }
}

fn exit_status(process: HANDLE) -> io::Result<ExitStatus> {
    let mut code = 0u32;
    // SAFETY: `code` is a valid out pointer.
    if unsafe { GetExitCodeProcess(process, &mut code) } == 0 {
        return Err(os_error());
    }
    Ok(ExitStatus::from_raw(code))
}

fn owned(handle: HANDLE) -> OwnedHandle {
    // SAFETY: the caller passes a valid handle it owns, and gives it up here.
    unsafe { OwnedHandle::from_raw_handle(handle) }
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}

/// The id and creation time of this process.
pub fn current_identity() -> io::Result<ProcessIdentity> {
    // SAFETY: the pseudo handle of the calling process is always valid.
    let started = creation_time(unsafe { GetCurrentProcess() })?;
    // SAFETY: no arguments.
    let pid = unsafe { GetCurrentProcessId() };
    Ok(ProcessIdentity { pid, started })
}

/// Whether process `pid` is running and was created at `started`.
///
/// False when no such process exists, when it has exited, or when the id now
/// belongs to a newer process. True when it cannot tell (access denied, an
/// unreadable creation time): the journal's keep set errs toward keeping.
pub fn process_alive(pid: u32, started: u64) -> bool {
    // SAFETY: a bad id only fails the call.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return os_error().raw_os_error() != Some(ERROR_INVALID_PARAMETER);
    }
    let handle = owned(handle);
    match creation_time(raw(&handle)) {
        Ok(created) if created != started => return false,
        Ok(_) => {}
        Err(_) => return true,
    }
    match wait_raw(raw(&handle), 0) {
        Ok(exited) => !exited,
        Err(_) => true,
    }
}

/// A process we hold a handle to: it can be waited for and ended without
/// going through its id, so a reused id can never be hit.
#[derive(Debug)]
pub struct ProcessHandle {
    handle: OwnedHandle,
    identity: ProcessIdentity,
}

impl ProcessHandle {
    /// Opens process `pid` if it is the one created at `started`. `None` when
    /// it does not exist or the id now belongs to another process.
    pub fn open(pid: u32, started: u64) -> io::Result<Option<Self>> {
        Ok(Self::open_pid(pid)?.filter(|process| process.identity.started == started))
    }

    /// Opens process `pid` and reads its creation time. `None` when it does
    /// not exist.
    pub fn open_pid(pid: u32) -> io::Result<Option<Self>> {
        // SAFETY: a bad id only fails the call.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            let error = os_error();
            return match error.raw_os_error() {
                Some(ERROR_INVALID_PARAMETER) => Ok(None),
                _ => Err(error),
            };
        }
        let handle = owned(handle);
        let started = creation_time(raw(&handle))?;
        Ok(Some(Self {
            handle,
            identity: ProcessIdentity { pid, started },
        }))
    }

    pub fn identity(&self) -> ProcessIdentity {
        self.identity
    }

    pub fn pid(&self) -> u32 {
        self.identity.pid
    }

    /// The exit status if the process has exited, without waiting.
    pub fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        if wait_raw(raw(&self.handle), 0)? {
            exit_status(raw(&self.handle)).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Blocks until the process exits.
    pub fn wait(&self) -> io::Result<ExitStatus> {
        wait_raw(raw(&self.handle), INFINITE)?;
        exit_status(raw(&self.handle))
    }

    /// Waits up to `timeout`; `None` when the process is still running.
    pub fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        if wait_raw(raw(&self.handle), millis.min(u32::MAX - 1))? {
            exit_status(raw(&self.handle)).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Waits for the process to exit without holding a thread: the system
    /// signals a channel when the handle is signalled. Dropping the future
    /// cancels the wait.
    pub async fn wait_async(&self) -> io::Result<ExitStatus> {
        if let Some(status) = self.try_wait()? {
            return Ok(status);
        }
        let mut watch = ExitWatch::new(&self.handle)?;
        if let Some(signalled) = watch.signalled.take() {
            // A closed channel is not an exit; the wait below settles it.
            let _ = signalled.await;
        }
        drop(watch);
        self.wait()
    }

    /// Ends the process. Succeeds when it had already exited.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: a bad handle or a dead process only fails the call.
        if unsafe { TerminateProcess(raw(&self.handle), exit_code) } != 0 {
            return Ok(());
        }
        let error = os_error();
        match self.try_wait() {
            Ok(Some(_)) => Ok(()),
            _ => Err(error),
        }
    }
}

struct WatchState {
    sender: Mutex<Option<oneshot::Sender<()>>>,
    fired: AtomicBool,
}

/// A registered wait on a process handle. The handles are kept as integers so
/// the value can be held across an `await`.
struct ExitWatch {
    registration: usize,
    context: usize,
    state: Arc<WatchState>,
    signalled: Option<oneshot::Receiver<()>>,
    _handle: OwnedHandle,
}

unsafe extern "system" fn on_exit(context: *mut c_void, _timed_out: bool) {
    // SAFETY: `context` came from `Arc::into_raw` in `ExitWatch::new`, and the
    // wait runs once, so this takes back the one count that was given away.
    let state = unsafe { Arc::from_raw(context as *const WatchState) };
    state.fired.store(true, Ordering::SeqCst);
    if let Ok(mut sender) = state.sender.lock()
        && let Some(sender) = sender.take()
    {
        let _ = sender.send(());
    }
}

impl ExitWatch {
    fn new(process: &OwnedHandle) -> io::Result<Self> {
        let mut copy: HANDLE = null_mut();
        // SAFETY: duplicates a handle of this process into this process.
        let ok = unsafe {
            let current = GetCurrentProcess();
            DuplicateHandle(
                current,
                raw(process),
                current,
                &mut copy,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(os_error());
        }
        let copy = owned(copy);
        let (sender, receiver) = oneshot::channel();
        let state = Arc::new(WatchState {
            sender: Mutex::new(Some(sender)),
            fired: AtomicBool::new(false),
        });
        let context = Arc::into_raw(Arc::clone(&state));
        let mut registration: HANDLE = null_mut();
        // SAFETY: `context` stays valid until the callback or `drop` takes it
        // back, and `copy` stays open until after the wait is unregistered.
        let ok = unsafe {
            RegisterWaitForSingleObject(
                &mut registration,
                raw(&copy),
                Some(on_exit),
                context.cast::<c_void>(),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        };
        if ok == 0 {
            let error = os_error();
            // SAFETY: the wait was not registered, so nothing else holds this count.
            drop(unsafe { Arc::from_raw(context) });
            return Err(error);
        }
        Ok(Self {
            registration: registration as usize,
            context: context as usize,
            state,
            signalled: Some(receiver),
            _handle: copy,
        })
    }
}

impl Drop for ExitWatch {
    fn drop(&mut self) {
        // SAFETY: waits for a running callback to finish, then the wait is gone.
        unsafe {
            UnregisterWaitEx(self.registration as HANDLE, INVALID_HANDLE_VALUE);
        }
        if !self.state.fired.load(Ordering::SeqCst) {
            // SAFETY: the callback never ran, so its count is still ours to drop.
            drop(unsafe { Arc::from_raw(self.context as *const WatchState) });
        }
    }
}

/// A job object that kills every process in it when its last handle closes.
/// Dropping the `Job` ends everything still in it.
#[derive(Debug)]
pub struct Job {
    handle: OwnedHandle,
}

impl Job {
    pub fn create_kill_on_close() -> io::Result<Self> {
        // SAFETY: no security attributes and no name.
        let handle = unsafe { CreateJobObjectW(null(), null()) };
        if handle.is_null() {
            return Err(os_error());
        }
        let job = Self {
            handle: owned(handle),
        };
        // SAFETY: plain data, set to all zeros and then given its one flag.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the pointer and length describe `limits`.
        let ok = unsafe {
            SetInformationJobObject(
                raw(&job.handle),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(os_error());
        }
        Ok(job)
    }

    /// Puts a process in the job. The handle needs the set-quota and
    /// terminate rights, which the handle `CreateProcessW` returns has.
    fn assign(&self, process: HANDLE) -> io::Result<()> {
        // SAFETY: a bad handle only fails the call.
        if unsafe { AssignProcessToJobObject(raw(&self.handle), process) } == 0 {
            return Err(os_error());
        }
        Ok(())
    }

    /// How many processes are in the job now.
    pub fn active_processes(&self) -> io::Result<u32> {
        // SAFETY: plain data that the call fills in.
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        // SAFETY: the pointer and length describe `accounting`.
        let ok = unsafe {
            QueryInformationJobObject(
                raw(&self.handle),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast::<c_void>(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(os_error());
        }
        Ok(accounting.ActiveProcesses)
    }

    /// Ends every process in the job, grandchildren included.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: a bad handle only fails the call.
        if unsafe { TerminateJobObject(raw(&self.handle), exit_code) } == 0 {
            return Err(os_error());
        }
        Ok(())
    }
}

/// What to start. Standard input is always the null device.
#[derive(Debug, Clone, Copy)]
pub struct SpawnSpec<'a> {
    pub exe: &'a Path,
    pub args: &'a [std::ffi::OsString],
    pub cwd: Option<&'a Path>,
    /// The worker's standard output; the null device when `None`.
    pub stdout: Option<&'a File>,
    /// The worker's standard error; the same file as `stdout` when `None`.
    pub stderr: Option<&'a File>,
}

/// A running worker, its handle and the job it and its children live in.
///
/// The top-level runner owns it above `catch_unwind`: it stops the worker
/// itself (a graceful stop or EOF on the control pipe, a bounded wait, then
/// [`kill`](Self::kill)). `Drop` is the emergency path only: it closes the
/// job, which ends the worker and everything it started, and warns with the
/// worker's id if the worker was still running. Keep it for as long as the
/// worker should live. `try_wait` and `wait_timeout` do not consume it.
#[derive(Debug)]
pub struct WorkerProcess {
    process: ProcessHandle,
    job: Job,
}

impl WorkerProcess {
    /// Starts the worker so that it never runs outside a kill-on-close job,
    /// and inherits only its three standard handles.
    ///
    /// In order: make the job (a failure starts nothing); make the inheritable
    /// standard handles; `CreateProcessW` suspended with an explicit list of
    /// handles to inherit (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`); put the
    /// process in the job; resume it. If any step after the process exists
    /// fails, the process is ended and the error returned: it fails closed.
    pub fn spawn(spec: &SpawnSpec<'_>) -> io::Result<Self> {
        let job = Job::create_kill_on_close()?;
        let application = wide_z(spec.exe.as_os_str())?;
        let mut command_line = command_line(spec)?;
        let directory = spec.cwd.map(|path| wide_z(path.as_os_str())).transpose()?;

        let serial = SPAWN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stdin = open_nul()?;
        let stdout = match spec.stdout {
            Some(file) => inheritable_copy(file)?,
            None => open_nul()?,
        };
        let stderr = match spec.stderr.or(spec.stdout) {
            Some(file) => inheritable_copy(file)?,
            None => open_nul()?,
        };
        let inherited = [raw(&stdin), raw(&stdout), raw(&stderr)];
        let mut attributes = AttributeList::inheriting(&inherited)?;

        // SAFETY: plain data; every field the call reads is set below.
        let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = inherited[0];
        startup.StartupInfo.hStdOutput = inherited[1];
        startup.StartupInfo.hStdError = inherited[2];
        startup.lpAttributeList = attributes.as_mut_ptr();
        // SAFETY: plain data that the call fills in.
        let mut created: PROCESS_INFORMATION = unsafe { zeroed() };

        // SAFETY: every pointer is valid and nul-terminated where it must be;
        // `command_line` is writable; the attribute list and the handle array
        // it points at outlive the call.
        let ok = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW,
                null(),
                directory.as_ref().map_or(null(), |path| path.as_ptr()),
                (&startup as *const STARTUPINFOEXW).cast(),
                &mut created,
            )
        };
        if ok == 0 {
            return Err(os_error());
        }
        let mut process = ProcessHandle {
            handle: owned(created.hProcess),
            identity: ProcessIdentity {
                pid: created.dwProcessId,
                started: 0,
            },
        };
        let thread = owned(created.hThread);
        drop((stdin, stdout, stderr));
        drop(attributes);
        drop(serial);

        let started = (|| {
            process.identity.started = creation_time(raw(&process.handle))?;
            job.assign(raw(&process.handle))?;
            // SAFETY: resumes the suspended first thread of the new process.
            if unsafe { ResumeThread(raw(&thread)) } == u32::MAX {
                return Err(os_error());
            }
            Ok(())
        })();
        match started {
            Ok(()) => Ok(Self { process, job }),
            Err(error) => {
                // Still suspended, and possibly outside the job: end it directly.
                let _ = process.terminate(KILLED);
                let _ = job.terminate(KILLED);
                Err(error)
            }
        }
    }

    pub fn pid(&self) -> u32 {
        self.process.pid()
    }

    pub fn identity(&self) -> ProcessIdentity {
        self.process.identity()
    }

    pub fn process(&self) -> &ProcessHandle {
        &self.process
    }

    pub fn job(&self) -> &Job {
        &self.job
    }

    pub fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        self.process.try_wait()
    }

    pub fn wait(&self) -> io::Result<ExitStatus> {
        self.process.wait()
    }

    pub fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        self.process.wait_timeout(timeout)
    }

    pub async fn wait_async(&self) -> io::Result<ExitStatus> {
        self.process.wait_async().await
    }

    /// Ends the worker and everything in its job, by handle. Succeeds if
    /// either the job or the process could be ended.
    pub fn terminate(&self) -> io::Result<()> {
        let job = self.job.terminate(KILLED);
        let process = self.process.terminate(KILLED);
        job.or(process)
    }

    /// [`terminate`](Self::terminate), then waits up to `timeout` for the exit.
    pub fn kill_and_wait(&self, timeout: Duration) -> io::Result<ExitStatus> {
        self.terminate()?;
        self.process.wait_timeout(timeout)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "the worker did not exit after it was terminated",
            )
        })
    }

    /// Ends the worker and everything in its job, then waits on the retained
    /// handle until the worker has exited, so the caller knows it is gone.
    /// Does not consume the worker; safe to call again (an exited worker
    /// returns its status).
    pub fn kill(&self) -> io::Result<ExitStatus> {
        match self.terminate() {
            // Wait only after the worker was really ended, so this cannot hang.
            Ok(()) => self.process.wait(),
            // Terminating an exited worker fails; that is a repeat kill.
            Err(error) => match self.process.try_wait() {
                Ok(Some(status)) => Ok(status),
                _ => Err(error),
            },
        }
    }
}

/// Emergency only: the runner's owner is meant to stop the worker (graceful
/// stop, then [`WorkerProcess::kill`]) before it lets go. If the worker is
/// still running here, closing the job kills it and everything it started, and
/// that is logged with the worker's id. A worker that already exited is
/// silent, even if the job still holds a straggler that is now cleaned up.
impl Drop for WorkerProcess {
    fn drop(&mut self) {
        if matches!(self.process.try_wait(), Ok(None)) {
            tracing::warn!(
                pid = self.process.pid(),
                "dropping a running worker; its job is closed and it is force-killed"
            );
        }
        // The job handle closes after this, which ends whatever is left in it.
    }
}

/// A process thread attribute list holding one `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
struct AttributeList {
    buffer: Vec<usize>,
}

impl AttributeList {
    /// The list that lets a child inherit exactly `handles`. The slice must
    /// outlive the list, which keeps a pointer to it.
    fn inheriting(handles: &[HANDLE]) -> io::Result<Self> {
        let mut size = 0usize;
        // SAFETY: asks for the size; this call is expected to fail with
        // ERROR_INSUFFICIENT_BUFFER and set `size`.
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
        }
        if size == 0 {
            return Err(os_error());
        }
        let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
        // SAFETY: the buffer holds at least `size` bytes, suitably aligned.
        let ok = unsafe {
            InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast::<c_void>(), 1, 0, &mut size)
        };
        if ok == 0 {
            return Err(os_error());
        }
        let mut list = Self { buffer };
        // SAFETY: the list is initialised, and the handle array outlives it.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                list.as_mut_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast::<c_void>(),
                size_of_val(handles),
                null_mut(),
                null(),
            )
        };
        if ok == 0 {
            return Err(os_error());
        }
        Ok(list)
    }

    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.buffer.as_mut_ptr().cast::<c_void>()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: the list was initialised when this value was made.
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

/// The null device, opened so a child can inherit it.
fn open_nul() -> io::Result<OwnedHandle> {
    let name = wide_z(OsStr::new("NUL"))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    // SAFETY: `name` is nul-terminated and `attributes` is valid for the call.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(os_error());
    }
    Ok(owned(handle))
}

/// A second, inheritable handle to the same file. The caller's own handle is
/// left as it was.
fn inheritable_copy(file: &File) -> io::Result<OwnedHandle> {
    let mut copy: HANDLE = null_mut();
    // SAFETY: duplicates a handle of this process into this process.
    let ok = unsafe {
        let current = GetCurrentProcess();
        DuplicateHandle(
            current,
            file.as_raw_handle(),
            current,
            &mut copy,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(os_error());
    }
    Ok(owned(copy))
}

fn wide_z(text: &OsStr) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = text.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a path or argument has a nul character",
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// The command line: the executable quoted, then each argument quoted by the
/// rules `CommandLineToArgvW` and the C runtime undo. Nul-terminated.
fn command_line(spec: &SpawnSpec<'_>) -> io::Result<Vec<u16>> {
    let mut line = Vec::new();
    push_arg(&mut line, spec.exe.as_os_str(), true)?;
    for arg in spec.args {
        line.push(u16::from(b' '));
        push_arg(&mut line, arg, false)?;
    }
    if line.len() >= 32_767 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the worker's command line is too long",
        ));
    }
    line.push(0);
    Ok(line)
}

fn push_arg(line: &mut Vec<u16>, arg: &OsStr, force_quotes: bool) -> io::Result<()> {
    let wide: Vec<u16> = arg.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a path or argument has a nul character",
        ));
    }
    let special = |c: &u16| [b' ', b'\t', b'\n', 0x0b, b'"'].contains(&(*c as u8)) && *c < 0x80;
    if !force_quotes && !wide.is_empty() && !wide.iter().any(special) {
        line.extend_from_slice(&wide);
        return Ok(());
    }
    let (quote, backslash) = (u16::from(b'"'), u16::from(b'\\'));
    line.push(quote);
    let mut backslashes = 0usize;
    for &unit in &wide {
        if unit == backslash {
            backslashes += 1;
            continue;
        }
        if unit == quote {
            line.extend(std::iter::repeat_n(backslash, backslashes * 2 + 1));
        } else {
            line.extend(std::iter::repeat_n(backslash, backslashes));
        }
        backslashes = 0;
        line.push(unit);
    }
    line.extend(std::iter::repeat_n(backslash, backslashes * 2));
    line.push(quote);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn quoted(arg: &str) -> String {
        let mut line = Vec::new();
        push_arg(&mut line, OsStr::new(arg), false).unwrap();
        String::from_utf16(&line).unwrap()
    }

    #[test]
    fn arguments_are_quoted_the_way_the_c_runtime_undoes() {
        // Not upstream's: a wrong rule here changes what the worker is told.
        assert_eq!(quoted("plain"), "plain");
        assert_eq!(quoted(""), "\"\"");
        assert_eq!(quoted("two words"), "\"two words\"");
        assert_eq!(quoted("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quoted("C:\\dir with space\\"), "\"C:\\dir with space\\\\\"");
        assert_eq!(quoted("C:\\plain\\"), "C:\\plain\\");
        assert_eq!(quoted("a\\\\\"b c"), "\"a\\\\\\\\\\\"b c\"");
    }

    #[test]
    fn a_nul_in_an_argument_is_refused() {
        // Not upstream's: a nul would cut the command line short.
        let mut line = Vec::new();
        assert!(push_arg(&mut line, OsStr::new("a\0b"), false).is_err());
        let spec = SpawnSpec {
            exe: Path::new("C:\\x.exe"),
            args: &[OsString::from("a\0b")],
            cwd: None,
            stdout: None,
            stderr: None,
        };
        assert!(command_line(&spec).is_err());
    }

    #[test]
    fn this_process_is_alive_and_its_id_with_another_start_is_not() {
        // Not upstream's: the keep set counts a reused id as a dead process.
        let me = current_identity().unwrap();
        assert!(process_alive(me.pid, me.started));
        assert!(!process_alive(me.pid, me.started + 1));
        assert!(!process_alive(u32::MAX, 0));
    }

    #[test]
    fn open_checks_the_start_time() {
        // Not upstream's: a handle is only given for the process that was recorded.
        let me = current_identity().unwrap();
        assert!(ProcessHandle::open(me.pid, me.started).unwrap().is_some());
        assert!(
            ProcessHandle::open(me.pid, me.started + 1)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_missing_program_fails_to_start_and_leaves_nothing() {
        // Not upstream's: a failed spawn must report the error, not a half worker.
        let spec = SpawnSpec {
            exe: Path::new("C:\\no\\such\\dir\\open-ferry-missing.exe"),
            args: &[],
            cwd: None,
            stdout: None,
            stderr: None,
        };
        assert!(WorkerProcess::spawn(&spec).is_err());
    }
}
