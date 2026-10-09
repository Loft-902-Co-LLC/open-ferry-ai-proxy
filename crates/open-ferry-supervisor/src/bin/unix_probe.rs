//! Process-test helper, not a server entry point.
#[cfg(unix)]
fn main() -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let args: Vec<String> = std::env::args().collect();
    if args
        .get(1)
        .is_some_and(|arg| arg == "--closed-stdio-spawn-error")
    {
        for fd in 0..3 {
            unsafe {
                libc::close(fd);
            }
        }
        let a = std::net::TcpListener::bind("127.0.0.1:0")?;
        let b = std::net::TcpListener::bind("127.0.0.1:0")?;
        let dir = args
            .get(2)
            .ok_or_else(|| std::io::Error::other("missing directory"))?;
        let command = std::process::Command::new(std::path::Path::new(dir).join("missing-worker"));
        return match open_ferry_supervisor::process::unix::WorkerProcess::spawn(command, &[a, b]) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(std::io::Error::other(
                "exec error was lost while passing descriptors",
            )),
        };
    }
    if args.get(1).is_some_and(|arg| arg == "--collision") {
        use std::io::{BufRead, Read, Write};
        let a = std::net::TcpListener::bind("127.0.0.1:0")?;
        let b = std::net::TcpListener::bind("127.0.0.1:0")?;
        if a.as_raw_fd() != 3 || b.as_raw_fd() != 4 {
            return Err(std::io::Error::other(
                "collision probe did not obtain fd 3 and 4",
            ));
        }
        let address = a.local_addr()?;
        let management_address = b.local_addr()?;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .arg(
                args.get(2)
                    .ok_or_else(|| std::io::Error::other("missing directory"))?,
            )
            .arg("collision");
        let (mut worker, mut control) =
            open_ferry_supervisor::process::unix::WorkerProcess::spawn(command, &[a, b])?;
        control.set_nonblocking(false)?;
        control.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        let mut reader = std::io::BufReader::new(control.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if !line.contains("loaded") {
            return Err(std::io::Error::other("worker failed to load"));
        }
        control.write_all(b"{\"type\":\"take_over\"}\n")?;
        line.clear();
        reader.read_line(&mut line)?;
        if !line.contains("ready") {
            return Err(std::io::Error::other("worker failed to take over"));
        }
        for (address, expected) in [
            (address, "collision:main"),
            (management_address, "collision:management"),
        ] {
            let mut client = std::net::TcpStream::connect(address)?;
            client.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
            let mut response = String::new();
            client.read_to_string(&mut response)?;
            if response != expected {
                return Err(std::io::Error::other("wrong listener passed"));
            }
        }
        control.write_all(b"{\"type\":\"stop\",\"budget_ms\":100}\n")?;
        if !worker.wait()?.success() {
            return Err(std::io::Error::other("worker failed"));
        }
        return Ok(());
    }
    if args.get(1).is_some_and(|arg| arg == "--probe-inheritance") {
        for fd in args.iter().skip(2).filter_map(|fd| fd.parse::<i32>().ok()) {
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1 {
                return Err(std::io::Error::other(
                    "grandchild inherited a worker descriptor",
                ));
            }
        }
        return Ok(());
    }
    // SAFETY: first action, before threads or children; only our runner starts this helper.
    let (listeners, control) =
        unsafe { open_ferry_supervisor::sockets::unix::take_worker_sockets() }?;
    let mut fds: Vec<_> = listeners.iter().map(AsRawFd::as_raw_fd).collect();
    fds.push(control.as_raw_fd());
    let lock_path = args
        .get(1)
        .ok_or_else(|| std::io::Error::other("missing private test directory"))?;
    let lease = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(std::path::Path::new(lock_path).join("lease"))?;
    lease.try_lock().map_err(std::io::Error::other)?;
    let journal = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(std::path::Path::new(lock_path).join("journal"))?;
    fds.extend([lease.as_raw_fd(), journal.as_raw_fd()]);
    let mut probe = std::process::Command::new(std::env::current_exe()?);
    probe
        .arg("--probe-inheritance")
        .args(fds.iter().map(ToString::to_string));
    let mut child = probe.spawn()?;
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    if !status.success() {
        return Err(std::io::Error::other("inheritance probe failed"));
    }
    let marker = args.get(2).cloned().unwrap_or_else(|| "worker".into());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        use open_ferry_supervisor::protocol::{Channel, Message};
        use tokio::io::AsyncWriteExt;
        let control = tokio::net::UnixStream::from_std(control)?;
        let (reader, writer) = control.into_split();
        let mut channel = Channel::new(reader, writer);
        let mut listeners = listeners.into_iter();
        let retained = listeners.next().ok_or_else(|| std::io::Error::other("no main socket"))?;
        let management = listeners.next();
        let mut accepting: Option<tokio::net::TcpListener> = None;
        let mut accepting_management: Option<tokio::net::TcpListener> = None;
        channel.send(&Message::Loaded { ms: 0 }).await?;
        loop {
            tokio::select! {
                message = channel.receive::<Message>() => {
                    match message?.map(|frame| frame.message) {
                        Some(Message::TakeOver | Message::Resume) => {
                            accepting = Some(tokio::net::TcpListener::from_std(retained.try_clone()?)?);
                            accepting_management = management.as_ref().map(|socket| {
                                tokio::net::TcpListener::from_std(socket.try_clone()?)
                            }).transpose()?;
                            channel.send(&Message::Ready).await?;
                        }
                        Some(Message::HandOver { .. }) => {
                            accepting = None;
                            accepting_management = None;
                            channel.send(&Message::HandedOver { state_id: "probe".into(), live: 0 }).await?;
                        }
                        Some(Message::Stop { .. }) | None => return Ok(()),
                        _ => return Err(std::io::Error::other("unexpected helper command")),
                    }
                }
                accepted = async {
                    tokio::select! {
                        accepted = accept_optional(accepting.as_ref()) => accepted.map(|pair| (pair, "main")),
                        accepted = accept_optional(accepting_management.as_ref()) => accepted.map(|pair| (pair, "management")),
                    }
                } => {
                    let ((mut stream, _), role) = accepted?;
                    let response = if management.is_some() { format!("{marker}:{role}") } else { marker.clone() };
                    stream.write_all(response.as_bytes()).await?;
                    stream.shutdown().await?;
                }
            }
        }
    })
}
#[cfg(unix)]
async fn accept_optional(
    listener: Option<&tokio::net::TcpListener>,
) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    match listener {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}
#[cfg(not(unix))]
fn main() {}
