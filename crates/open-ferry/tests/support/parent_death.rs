//! Test-only cleanup for the real supervisor's separate process group.
//! macOS nextest timeouts can leave the supervisor until the runner ends;
//! Windows children are covered by nextest's job object.
use std::os::unix::process::CommandExt;
use std::process::Command;

pub fn on_test_exit(command: &mut Command) {
    // SAFETY: getpid has no preconditions. Each caller spawns synchronously on
    // the test thread, which also owns and outlives the child's cleanup guard.
    let parent = unsafe { libc::getpid() };
    // SAFETY: the child performs only async-signal-safe system calls before
    // exec; failure exits directly, without allocation, locks or destructors.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 || libc::getppid() != parent
            {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}
