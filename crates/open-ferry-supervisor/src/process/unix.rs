//! Collision-safe fd passing. The post-fork hook only calls fcntl, dup2 and
//! (Linux) close_range; environment construction happens in the parent.
use super::ProcessIdentity;
use crate::sockets::unix::sources;
use std::{
    io,
    net::TcpListener,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, ExitStatus},
};

/// Owned worker capability. Phase 2 keeps it and its control channel above the
/// catch_unwind/JoinError boundary, sends Stop or closes control, and waits the
/// supplied shutdown budget before calling kill. Drop is emergency cleanup only:
/// it warns, kills and reaps a live worker, never a graceful drain path.
pub struct WorkerProcess {
    child: Child,
    identity: ProcessIdentity,
}
impl WorkerProcess {
    /// Adds LISTEN_FDS and the private control fd; caller supplies -worker,
    /// -config and other binary-specific arguments. Stdout/stderr stay as given.
    pub fn spawn(
        mut command: Command,
        listeners: &[TcpListener],
    ) -> io::Result<(Self, UnixStream)> {
        let (parent, child_end) = UnixStream::pair()?;
        parent.set_nonblocking(true)?;
        let copied = sources(listeners, &child_end)?;
        let mappings: Vec<_> = copied
            .iter()
            .enumerate()
            .map(|(index, source)| (source.as_raw_fd(), 3 + index as i32))
            .collect();
        // Reserve otherwise empty targets in the parent, so std's exec-error pipe
        // cannot allocate its writer there (even when the caller closed stdio).
        let mut reservations = Vec::new();
        for &(source, target) in &mappings {
            if unsafe { libc::fcntl(target, libc::F_GETFD) } < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EBADF) {
                    return Err(error);
                }
                let fd = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, target) };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: successful fcntl returned a fresh exclusively owned fd.
                reservations.push(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
            }
        }
        // Get the bound in the parent: sysconf is not called after fork.
        let fd_limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
        if fd_limit < 0 || fd_limit > i32::MAX as libc::c_long {
            return Err(io::Error::other("cannot establish descriptor limit"));
        }
        command
            .env("LISTEN_FDS", listeners.len().to_string())
            .env_remove("LISTEN_PID")
            .env(
                "LISTEN_FDNAMES",
                if listeners.len() == 1 {
                    "main"
                } else {
                    "main:management"
                },
            )
            .arg("-worker-control")
            .arg((3 + listeners.len()).to_string());
        // SAFETY: no Rust allocation/locking or environment work in this hook.
        // Sources were duplicated above all target fds, so dup2 cannot clobber
        // a later source even when original sockets/control occupied 3,4,5.
        unsafe {
            command.pre_exec(move || {
                mark_other_fds_cloexec(fd_limit as i32)?;
                for &(source, target) in &mappings {
                    if libc::dup2(source, target) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(reservations);
        drop(copied);
        drop(child_end);
        let started = match start_time(child.id()) {
            Ok(Some(started)) => started,
            Ok(None) => {
                let _ = child.wait();
                return Err(io::Error::other(
                    "worker exited before identity was captured",
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok((
            Self {
                identity: ProcessIdentity {
                    pid: child.id(),
                    started,
                },
                child,
            },
            parent,
        ))
    }
    pub fn identity(&self) -> ProcessIdentity {
        self.identity
    }
    pub fn pid(&self) -> u32 {
        self.identity.pid
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
    /// Bounded, non-consuming wait. Call from a blocking context, not a tokio worker.
    pub fn wait_timeout(&mut self, budget: std::time::Duration) -> io::Result<Option<ExitStatus>> {
        let start = std::time::Instant::now();
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(Some(status));
            }
            let Some(remaining) = budget.checked_sub(start.elapsed()) else {
                return Ok(None);
            };
            std::thread::sleep(remaining.min(std::time::Duration::from_millis(10)));
        }
    }
    /// Explicit emergency kill, followed by reap through the retained child.
    pub fn kill(&mut self) -> io::Result<ExitStatus> {
        self.terminate()
    }
    /// Uses the retained child, never a name or a process search.
    pub fn terminate(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.child.try_wait()? {
            return Ok(status);
        }
        self.child.kill()?;
        self.child.wait()
    }
}
impl Drop for WorkerProcess {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            tracing::warn!(
                pid = self.identity.pid,
                "worker dropped before graceful shutdown; killing and reaping"
            );
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// SAFETY: called only in pre_exec; no allocation and only async-signal-safe
// fcntl (close_range is a direct Linux syscall), before collision-free dup2.
unsafe fn mark_other_fds_cloexec(limit: i32) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL)) {
            return Err(error);
        }
    }
    for fd in 3..limit {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Opaque start identity: Linux boot-relative ticks, macOS epoch microseconds.
/// Unknown permissions/OS errors are live, retaining the version fail-safely.
pub fn process_alive(pid: u32, started: u64) -> bool {
    match start_time(pid) {
        Ok(Some(actual)) => actual == started,
        Ok(None) => false,
        Err(_) => true,
    }
}

#[cfg(target_os = "linux")]
pub fn start_time(pid: u32) -> io::Result<Option<u64>> {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => {
            let fields = text
                .rsplit_once(')')
                .map(|(_, tail)| tail.split_whitespace().collect::<Vec<_>>())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid process identity")
                })?;
            if fields
                .first()
                .is_some_and(|state| matches!(*state, "Z" | "X"))
            {
                return Ok(None);
            }
            let started = fields
                .get(19)
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "missing process start time")
                })?;
            Ok(Some(started))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "macos")]
pub fn start_time(pid: u32) -> io::Result<Option<u64>> {
    // proc_bsdinfo from Apple's libproc.h. No process enumeration or command line.
    #[repr(C)]
    struct BsdInfo {
        flags: u32,
        status: u32,
        xstatus: u32,
        pid: u32,
        ppid: u32,
        uid: u32,
        gid: u32,
        ruid: u32,
        rgid: u32,
        svuid: u32,
        svgid: u32,
        reserved: u32,
        comm: [u8; 16],
        name: [u8; 32],
        nfiles: u32,
        pgid: u32,
        jobc: u32,
        tdev: u32,
        tpgid: u32,
        nice: i32,
        start_sec: u64,
        start_usec: u64,
    }
    unsafe extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            size: libc::c_int,
        ) -> libc::c_int;
    }
    let pid = i32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid pid"))?;
    let mut info = std::mem::MaybeUninit::<BsdInfo>::zeroed();
    let size = std::mem::size_of::<BsdInfo>() as i32;
    // SAFETY: output buffer has the documented proc_bsdinfo layout and size.
    let read = unsafe { proc_pidinfo(pid, 3, 0, info.as_mut_ptr().cast(), size) };
    if read != size {
        if read > 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete process identity",
            ));
        }
        let error = io::Error::last_os_error();
        return if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let info = unsafe { info.assume_init() };
    if info.status == 5 {
        return Ok(None);
    } // SZOMB
    info.start_sec
        .checked_mul(1_000_000)
        .and_then(|s| s.checked_add(info.start_usec))
        .map(Some)
        .ok_or_else(|| io::Error::other("process start time overflow"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn start_time(_pid: u32) -> io::Result<Option<u64>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process identity unsupported on this Unix",
    ))
}
