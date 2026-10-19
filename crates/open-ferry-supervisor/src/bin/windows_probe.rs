//! Not upstream's: a helper process for the Windows plumbing tests. The tests
//! start it as a worker through the runner and talk to it through files in a
//! temporary directory, so nothing here touches a network but loopback or any
//! process but its own. Every mode ends itself after two minutes.

#[cfg(not(windows))]
fn main() {
    eprintln!("windows_probe only runs on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    std::process::exit(imp::run());
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::mem::zeroed;
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use open_ferry_supervisor::process::windows::current_identity;
    use open_ferry_supervisor::sockets::windows::listener_from_info;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Networking::WinSock::{SOCKET, WSADATA, WSAStartup, getsockopt};
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

    const LIFETIME: Duration = Duration::from_secs(120);

    pub fn run() -> i32 {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match args.first().map(String::as_str) {
            Some("serve") => serve(&args),
            Some("report") => report(&args),
            Some("grandchild") => grandchild(&args),
            Some("sleep") => sleep(&args),
            Some("exit") => exit_with(&args),
            _ => {
                eprintln!("usage: windows_probe serve|report|grandchild|sleep|exit ...");
                2
            }
        }
    }

    fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
        let at = args.iter().position(|arg| arg == name)?;
        args.get(at + 1).map(String::as_str)
    }

    fn wait_for(path: &Path, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// A file that is whole whenever it exists.
    fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, path)
    }

    fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    }

    /// Serves one line, its process id, to each connection it accepts on the
    /// listener it was handed. A pause file makes it drop its tokio listener
    /// and keep only the std one; removing the file builds a new tokio
    /// listener from that. `<pause file>.state` says which it is doing.
    fn serve(args: &[String]) -> i32 {
        let (Some(info), Some(pause), Some(stop), Some(ready)) = (
            flag(args, "--info-file"),
            flag(args, "--pause-file"),
            flag(args, "--stop-file"),
            flag(args, "--ready-file"),
        ) else {
            eprintln!("serve needs --info-file --pause-file --stop-file --ready-file");
            return 2;
        };
        let (info, pause, stop, ready) = (
            Path::new(info),
            Path::new(pause),
            Path::new(stop),
            Path::new(ready),
        );
        if !wait_for(info, Duration::from_secs(30)) {
            eprintln!("no socket info arrived");
            return 3;
        }
        let text = match std::fs::read_to_string(info) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("reading the socket info: {error}");
                return 3;
            }
        };
        let listener = match listener_from_info(text.trim()) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("opening the socket: {error}");
                return 4;
            }
        };
        if let Err(error) = listener.set_nonblocking(true) {
            eprintln!("set_nonblocking: {error}");
            return 4;
        }
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("runtime: {error}");
                return 5;
            }
        };
        runtime.block_on(serve_loop(listener, pause, stop, ready))
    }

    fn tokio_copy(listener: &TcpListener) -> io::Result<tokio::net::TcpListener> {
        // Non-blocking mode is not carried over to a clone, so set it on the copy.
        let copy = listener.try_clone()?;
        open_ferry_supervisor::sockets::windows::set_no_inherit(&copy)?;
        copy.set_nonblocking(true)?;
        tokio::net::TcpListener::from_std(copy)
    }

    async fn serve_loop(listener: TcpListener, pause: &Path, stop: &Path, ready: &Path) -> i32 {
        use tokio::io::AsyncWriteExt;
        let reply = format!("{}\n", std::process::id());
        let state = with_suffix(pause, ".state");
        let mut live = match tokio_copy(&listener) {
            Ok(live) => Some(live),
            Err(error) => {
                eprintln!("tokio listener: {error}");
                return 5;
            }
        };
        let _ = write_atomic(&state, "serving");
        let _ = write_atomic(ready, &reply);
        let end = Instant::now() + LIFETIME;
        while !stop.exists() && Instant::now() < end {
            if pause.exists() {
                if live.take().is_some() {
                    let _ = write_atomic(&state, "paused");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            if live.is_none() {
                match tokio_copy(&listener) {
                    Ok(next) => {
                        live = Some(next);
                        let _ = write_atomic(&state, "serving");
                    }
                    Err(error) => {
                        eprintln!("tokio listener: {error}");
                        return 6;
                    }
                }
            }
            let Some(current) = live.as_ref() else {
                continue;
            };
            match tokio::time::timeout(Duration::from_millis(20), current.accept()).await {
                Ok(Ok((mut stream, _))) => {
                    let _ = stream.write_all(reply.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
                Ok(Err(error)) => eprintln!("accept: {error}"),
                Err(_) => {}
            }
        }
        0
    }

    /// Says on standard output whether the handle values it was given exist in
    /// this process: a socket, and a file whose path ends with a given name.
    /// Standard error gets a line too, so the test can check both.
    fn report(args: &[String]) -> i32 {
        println!("pid={}", std::process::id());
        if let Some(handle) = flag(args, "--socket").and_then(|text| text.parse::<u64>().ok()) {
            let word = if is_socket(handle) {
                "valid"
            } else {
                "invalid"
            };
            println!("socket={word}");
        }
        if let (Some(handle), Some(name)) = (
            flag(args, "--file").and_then(|text| text.parse::<u64>().ok()),
            flag(args, "--name"),
        ) {
            let word = if is_file_named(handle, name) {
                "valid"
            } else {
                "invalid"
            };
            println!("file={word}");
        }
        eprintln!("stderr-ok");
        0
    }

    fn is_socket(handle: u64) -> bool {
        // SAFETY: WSADATA is plain data; a handle that is not a socket only
        // makes getsockopt fail.
        unsafe {
            let mut data: WSADATA = zeroed();
            WSAStartup(0x0202, &mut data);
            let mut value = 0i32;
            let mut length = 4i32;
            getsockopt(
                handle as usize as SOCKET,
                0xffff,
                0x1008,
                (&mut value as *mut i32).cast::<u8>(),
                &mut length,
            ) == 0
        }
    }

    fn is_file_named(handle: u64, name: &str) -> bool {
        let mut buffer = [0u16; 1024];
        // SAFETY: the buffer is valid for its length; a bad handle only fails the call.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                handle as usize as HANDLE,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                0,
            )
        } as usize;
        match buffer.get(..length) {
            Some(path) if length > 0 => String::from_utf16_lossy(path)
                .to_lowercase()
                .ends_with(&name.to_lowercase()),
            _ => false,
        }
    }

    /// Starts a `sleep` child, waits until it has written its identity, then
    /// sleeps too. Both are in the worker's job; the test ends them with it.
    fn grandchild(args: &[String]) -> i32 {
        let (Some(out), Some(stop)) = (flag(args, "--out"), flag(args, "--stop-file")) else {
            eprintln!("grandchild needs --out --stop-file");
            return 2;
        };
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                eprintln!("current_exe: {error}");
                return 3;
            }
        };
        let mut child = match Command::new(exe)
            .args(["sleep", "--out", out, "--stop-file", stop])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("spawn: {error}");
                return 3;
            }
        };
        if !wait_for(Path::new(out), Duration::from_secs(30)) {
            eprintln!("the child did not start");
            return 4;
        }
        wait_until_stopped(Path::new(stop));
        let _ = child.wait();
        0
    }

    fn wait_until_stopped(stop: &Path) {
        let end = Instant::now() + LIFETIME;
        while !stop.exists() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Writes `<pid> <started>` to `--out` if given, then waits for `--stop-file`.
    fn sleep(args: &[String]) -> i32 {
        if let Some(out) = flag(args, "--out") {
            let identity = match current_identity() {
                Ok(identity) => identity,
                Err(error) => {
                    eprintln!("identity: {error}");
                    return 3;
                }
            };
            let text = format!("{} {}", identity.pid, identity.started);
            if let Err(error) = write_atomic(Path::new(out), &text) {
                eprintln!("writing {out}: {error}");
                return 3;
            }
        }
        match flag(args, "--stop-file") {
            Some(stop) => wait_until_stopped(Path::new(stop)),
            None => std::thread::sleep(LIFETIME),
        }
        0
    }

    fn exit_with(args: &[String]) -> i32 {
        flag(args, "--code")
            .and_then(|text| text.parse::<i32>().ok())
            .unwrap_or(2)
    }
}
