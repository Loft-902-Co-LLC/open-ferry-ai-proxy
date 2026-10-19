//! Unix workers receive fd 3 onwards with LISTEN_FDS, but never LISTEN_PID.
//! Only a supervisor-created worker may consume these descriptors.
use std::{
    io,
    net::TcpListener,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::net::UnixStream,
};

pub(crate) fn cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl does not take ownership of the live descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn duplicate(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: a successful call returns a new descriptor owned by this function.
    let copied = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 6) };
    if copied < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(copied) })
    }
}

/// Take the fd range reserved by the runner and restore close-on-exec before
/// any worker thread or child starts.
///
/// # Safety
/// The caller exclusively owns the supplied inherited descriptors (3 onwards),
/// and must call this only at worker startup, before starting any threads.
pub unsafe fn take_listeners(count: usize) -> io::Result<Vec<TcpListener>> {
    if !(1..=2).contains(&count) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected main and optional management socket",
        ));
    }
    let mut result = Vec::with_capacity(count);
    let mut owned = Vec::with_capacity(count);
    for offset in 0..count {
        let fd = 3 + offset as RawFd;
        // SAFETY: the caller supplies exclusively owned, valid inherited fds.
        owned.push(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    for descriptor in &owned {
        cloexec(descriptor.as_raw_fd())?;
    }
    for descriptor in owned {
        let fd = descriptor.as_raw_fd();
        let mut address = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
        let mut address_size = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        // SAFETY: sockaddr_storage is large enough for either TCP address family.
        if unsafe { libc::getsockname(fd, address.as_mut_ptr().cast(), &mut address_size) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let family = unsafe { address.assume_init() }.ss_family as i32;
        if family != libc::AF_INET && family != libc::AF_INET6 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "inherited socket is not TCP",
            ));
        }
        if !is_listening(fd)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "inherited fd is not a listening socket",
            ));
        }
        let listener = TcpListener::from(descriptor);
        listener.set_nonblocking(true)?;
        result.push(listener);
    }
    Ok(result)
}

#[cfg(target_os = "macos")]
fn is_listening(fd: RawFd) -> io::Result<bool> {
    // Darwin does not implement getsockopt(SO_ACCEPTCONN). Its TCP state query
    // establishes the same fact without accepting or changing the socket.
    const TCPS_LISTEN: u8 = 1;
    // SAFETY: tcp_connection_info contains integer fields and padding.
    let mut info: libc::tcp_connection_info = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&info) as libc::socklen_t;
    // SAFETY: info and size are valid output storage for this query.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_CONNECTION_INFO,
            (&mut info as *mut libc::tcp_connection_info).cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(size > 0 && info.tcpi_state == TCPS_LISTEN)
}

#[cfg(not(target_os = "macos"))]
fn is_listening(fd: RawFd) -> io::Result<bool> {
    let mut listening: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&listening) as libc::socklen_t;
    // SAFETY: initialized output and size point to valid storage.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ACCEPTCONN,
            (&mut listening as *mut libc::c_int).cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(listening != 0)
}

/// Consume LISTEN_FDS and the private control descriptor at single-threaded startup.
///
/// # Safety
/// As for take_listeners; the environment and descriptors must have been supplied
/// by our runner, and no threads may be running while the variables are removed.
pub unsafe fn take_worker_sockets() -> io::Result<(Vec<TcpListener>, UnixStream)> {
    let count = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| (1..=2).contains(n))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing worker LISTEN_FDS"))?;
    let control_fd = 3 + count as RawFd;
    cloexec(control_fd)?;
    // SAFETY: successful fcntl above established a valid fd; startup caller owns it.
    let control = unsafe { OwnedFd::from_raw_fd(control_fd) };
    let mut address = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut size = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: valid fd and sufficiently large initialized output storage.
    if unsafe { libc::getpeername(control.as_raw_fd(), address.as_mut_ptr().cast(), &mut size) }
        != 0
    {
        return Err(io::Error::last_os_error());
    }
    let family = unsafe { address.assume_init() }.ss_family as i32;
    let mut kind: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&kind) as libc::socklen_t;
    // SAFETY: kind and size are valid writable output storage.
    if unsafe {
        libc::getsockopt(
            control.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if family != libc::AF_UNIX || kind != libc::SOCK_STREAM {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker control is not a connected Unix stream",
        ));
    }
    let control = UnixStream::from(control);
    let listeners = unsafe { take_listeners(count) }?;
    // SAFETY: required single-threaded startup contract.
    unsafe {
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_PID");
        std::env::remove_var("LISTEN_FDNAMES");
    }
    control.set_nonblocking(true)?;
    Ok((listeners, control))
}

pub(crate) fn sources(listeners: &[TcpListener], control: &UnixStream) -> io::Result<Vec<OwnedFd>> {
    if !(1..=2).contains(&listeners.len()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected main and optional management socket",
        ));
    }
    listeners
        .iter()
        .map(|s| duplicate(s.as_raw_fd()))
        .chain(std::iter::once(duplicate(control.as_raw_fd())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listener_is_distinguished_from_connected_and_bound_streams() {
        // Not upstream's: SO_ACCEPTCONN is not available on Darwin, and a TCP fd alone
        // does not prove that the inherited socket is listening.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(is_listening(listener.as_raw_fd()).unwrap());
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        assert!(!is_listening(stream.as_raw_fd()).unwrap());
        let bound =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        bound
            .bind(
                &"127.0.0.1:0"
                    .parse::<std::net::SocketAddr>()
                    .unwrap()
                    .into(),
            )
            .unwrap();
        assert!(!is_listening(bound.as_raw_fd()).unwrap());
    }
}
