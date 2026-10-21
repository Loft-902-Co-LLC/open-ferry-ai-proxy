//! Keep a listener's raised descriptor allowance out of its helper processes.

#[cfg(unix)]
static ORIGINAL_NOFILE: std::sync::OnceLock<libc::rlimit> = std::sync::OnceLock::new();

#[cfg(unix)]
pub fn remember_nofile_limit(limit: libc::rlimit) {
    let _ = ORIGINAL_NOFILE.set(limit);
}

/// Register in the parent. Child-side observation/restoration is best-effort.
pub fn restore_in_child(command: &mut std::process::Command) {
    #[cfg(unix)]
    if let Some(limit) = ORIGINAL_NOFILE.get().copied() {
        set_child_limit(command, limit);
    }
    #[cfg(not(unix))]
    let _ = command;
}

#[cfg(any(unix,test))]
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
struct Limit { soft:u64, hard:u64 }

// Shared by the real post-fork hook and portable syscall-policy regressions.
#[cfg(any(unix,test))]
fn restore_with(original: Limit, get: impl FnOnce()->Result<Limit,i32>, set: impl FnOnce(Limit)->Result<(),i32>) -> std::io::Result<()> {
    if let Ok(current) = get() {
        let next=Limit {soft:current.soft.min(original.soft),hard:current.hard};
        let _ = set(next);
    }
    Ok(())
}

#[cfg(unix)]
fn set_child_limit(command: &mut std::process::Command, limit: libc::rlimit) {
    use std::os::unix::process::CommandExt;
    let original=Limit {soft:limit.rlim_cur,hard:limit.rlim_max};
    // SAFETY: callbacks perform only resource-limit syscalls and errno reads;
    // the post-fork path allocates nothing and acquires no lock.
    unsafe {
        command.pre_exec(move || restore_with(original,|| {
            let mut current=libc::rlimit {rlim_cur:0,rlim_max:0};
            if libc::getrlimit(libc::RLIMIT_NOFILE,&mut current)==0 { Ok(Limit {soft:current.rlim_cur,hard:current.rlim_max}) }
            else { Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL)) }
        },|next| {
            let next=libc::rlimit {rlim_cur:next.soft,rlim_max:next.hard};
            if libc::setrlimit(libc::RLIMIT_NOFILE,&next)==0 {Ok(())}
            else {Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL))}
        }));
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use std::cell::Cell;
    #[test]
    fn a_child_keeps_its_inherited_hard_limit_after_a_later_reduction() {
        let original=Limit {soft:256,hard:1024};
        let inherited=Limit {soft:512,hard:512};
        let observed=Cell::new(None);
        let result=restore_with(original,||Ok(inherited),|next| {
            observed.set(Some(next));
            if next.hard>inherited.hard {Err(1)} else {Ok(())}
        });
        assert!(result.is_ok(),"hard-limit reduction prevented exec");
        assert_eq!(observed.get(),Some(Limit {soft:256,hard:512}));
    }
    #[test]
    fn a_child_never_raises_a_soft_limit_lowered_after_listener_start() {
        let original=Limit {soft:256,hard:1024};
        let inherited=Limit {soft:128,hard:1024};
        let observed=Cell::new(None);
        restore_with(original,||Ok(inherited),|next| {observed.set(Some(next));Ok(())}).unwrap();
        assert_eq!(observed.get(),Some(inherited),"child restored a stale higher soft limit");
    }
    #[test]
    fn failed_child_limit_syscalls_do_not_prevent_exec() {
        let original=Limit {soft:256,hard:1024};
        for get in [Ok(Limit {soft:512,hard:1024}),Err(1)] {
            assert!(restore_with(original,||get,|_|Err(1)).is_ok(),"resource-limit failure escaped pre_exec");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn current() -> libc::rlimit {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
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
        let target = libc::rlimit {
            rlim_cur: before.rlim_cur.min(256),
            rlim_max: before.rlim_max,
        };
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "process_limits::tests::child_probe"])
            .env("OFP_NOFILE_CHILD_PROBE", target.rlim_cur.to_string());
        set_child_limit(&mut command, target);
        assert!(command.status().unwrap().success());
        assert_eq!(current().rlim_cur, before.rlim_cur);
    }
}
