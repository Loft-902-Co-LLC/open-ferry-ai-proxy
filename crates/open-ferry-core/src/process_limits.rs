//! Keep a listener's raised descriptor allowance out of its helper processes.

#[cfg(unix)]
static ORIGINAL_NOFILE: std::sync::OnceLock<libc::rlimit> = std::sync::OnceLock::new();

#[cfg(unix)]
pub fn remember_nofile_limit(limit: libc::rlimit) {
    let _ = ORIGINAL_NOFILE.set(limit);
}

/// Register in the parent, before spawning. The child performs only setrlimit.
pub fn restore_in_child(command: &mut std::process::Command) {
    #[cfg(unix)]
    if let Some(limit) = ORIGINAL_NOFILE.get().copied() { set_child_limit(command, limit); }
    #[cfg(not(unix))]
    let _ = command;
}

#[cfg(unix)]
fn set_child_limit(command: &mut std::process::Command, limit: libc::rlimit) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the post-fork closure allocates nothing and invokes only the
    // libc resource-limit syscall, before the helper's image is executed.
    unsafe { command.pre_exec(move || {
        if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 { Ok(()) }
        else { Err(std::io::Error::last_os_error()) }
    }); }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn current() -> libc::rlimit {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
        limit
    }
    #[test]
    fn child_probe() {
        if let Ok(expected) = std::env::var("OFP_NOFILE_CHILD_PROBE") {
            assert_eq!(current().rlim_cur, expected.parse::<u64>().unwrap());
        }
    }
    #[test]
    fn helper_limits_are_restored_without_changing_the_parent() {
        let before = current();
        let target = libc::rlimit { rlim_cur: before.rlim_cur.min(256), rlim_max: before.rlim_max };
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "process_limits::tests::child_probe"])
            .env("OFP_NOFILE_CHILD_PROBE", target.rlim_cur.to_string());
        set_child_limit(&mut command, target);
        assert!(command.status().unwrap().success());
        assert_eq!(current().rlim_cur, before.rlim_cur);
    }
}
