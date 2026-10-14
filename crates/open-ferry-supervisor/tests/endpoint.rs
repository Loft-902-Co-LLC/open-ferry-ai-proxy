//! Tests of the supervisor's endpoint through its public API. No TCP, and no real service: the
//! Unix tests use directories of their own under `/tmp`, the Windows tests a pipe named from a
//! temp directory.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::UnixListener as StdListener;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use open_ferry_supervisor::endpoint::{
        EndpointError, Identity, Location, Server, ServiceKind, connect,
    };
    use open_ferry_supervisor::process::SupervisorLock;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// Taken exclusively by the one test here that forks, around the fork, and shared by every
    /// other test for its whole run. A forked child holds a copy of every descriptor open at the
    /// fork until it execs (`spawn` returns after the exec), so without this a stale socket's
    /// listener or a dropped lock could live on in it and make another test's socket look live
    /// or its lock look held.
    static FORKS: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

    /// A fresh endpoint name under `/tmp`, removed again on drop. `/tmp` is the one sticky root
    /// the policy accepts for an endpoint's ancestry.
    struct Fixture {
        location: Location,
        dir: PathBuf,
        _data: tempfile::TempDir,
        lock: SupervisorLock,
        identity: Identity,
    }

    impl Fixture {
        fn new() -> Self {
            let name = format!(
                "ofp-endpoint-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            );
            let root = fs::canonicalize("/tmp").expect("canonical endpoint root");
            let location = Location::new(&root, name).expect("location");
            assert!(
                location.socket_path().as_os_str().len() <= 103,
                "SET-UP: canonical socket path exceeds 103 bytes: {}",
                location.socket_path().display()
            );
            let data = tempfile::tempdir().expect("data dir");
            let lock = SupervisorLock::try_acquire(data.path()).expect("lock");
            let dir = location.dir();
            let _ = fs::remove_dir_all(&dir);
            Self {
                location,
                dir,
                _data: data,
                lock,
                identity: Identity::current(ServiceKind::User).expect("identity"),
            }
        }

        fn bind(&self) -> Result<Server, EndpointError> {
            Server::bind(&self.location, &self.identity, &self.lock)
        }

        fn make_dir(&self, mode: u32) {
            fs::create_dir(&self.dir).expect("mkdir");
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(mode)).expect("chmod");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Ok(meta) = fs::symlink_metadata(&self.dir) {
                if meta.file_type().is_symlink() {
                    let _ = fs::remove_file(&self.dir);
                } else {
                    let _ = fs::remove_dir_all(&self.dir);
                }
            }
        }
    }

    /// A bound stream socket that was never made a listener. Even a concurrent
    /// fork retaining this fd cannot make a stream connect succeed. Using
    /// StdListener::bind here left a listening fd alive in fork-to-exec windows.
    fn stale_socket(path: &Path) -> std::os::fd::OwnedFd {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: socket returns a fresh owned descriptor; sockaddr is zeroed
        // and filled with a checked, NUL-terminated filesystem path.
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        assert!(fd >= 0);
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as _;
        let bytes = path.as_os_str().as_bytes();
        assert!(bytes.len() < address.sun_path.len());
        for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
            *slot = *byte as _;
        }
        let length = std::mem::size_of_val(&address) as libc::socklen_t;
        #[cfg(target_os = "macos")]
        {
            address.sun_len = length as _;
        }
        assert_eq!(
            unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&address as *const libc::sockaddr_un).cast(),
                    length,
                )
            },
            0
        );
        fd
    }

    #[tokio::test]
    async fn a_stale_fixture_refuses_connect_even_while_an_fd_is_retained() {
        let fx = Fixture::new();
        fx.make_dir(0o700);
        let fd = stale_socket(&fx.location.socket_path());
        fs::set_permissions(fx.location.socket_path(), fs::Permissions::from_mode(0o600)).unwrap();
        // A fork can retain the descriptor until exec; retention is deliberate here.
        assert!(
            matches!(std::os::unix::net::UnixStream::connect(fx.location.socket_path()), Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused)
        );
        drop(fd);
        fx.bind().expect("replace the stale socket");
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o7777
    }

    #[tokio::test]
    async fn a_client_and_server_talk_over_a_private_socket() {
        // Not upstream's: one round trip, and the modes the policy demands.
        let _forks = FORKS.read().await;
        let fx = Fixture::new();
        let mut server = fx.bind().expect("bind");
        assert_eq!(mode_of(&fx.dir), 0o700);
        assert_eq!(mode_of(server.socket_path()), 0o600);

        let accept = tokio::spawn(async move {
            let mut connection = server.accept().await.expect("accept");
            let mut buf = [0u8; 4];
            connection.stream.read_exact(&mut buf).await.expect("read");
            connection.stream.write_all(&buf).await.expect("write");
            (connection.peer, server)
        });
        let mut client = connect(&fx.location, &fx.identity).await.expect("connect");
        client.write_all(b"ping").await.expect("write");
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.expect("read");
        assert_eq!(&echoed, b"ping");

        let (peer, server) = accept.await.expect("task");
        assert_eq!(peer.uid, Some(fx.identity.uid()));
        assert_eq!(server.rejected_peers(), 0);
    }

    #[tokio::test]
    async fn the_socket_is_removed_when_the_server_is_dropped() {
        // Not upstream's: a clean exit leaves no socket behind.
        let _forks = FORKS.read().await;
        let fx = Fixture::new();
        let server = fx.bind().expect("bind");
        let socket = server.socket_path().to_path_buf();
        assert!(socket.exists());
        drop(server);
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn the_location_survives_the_record() {
        // Not upstream's: supervisor.json's endpoint field.
        let _forks = FORKS.read().await;
        let fx = Fixture::new();
        let again = Location::from_record(&fx.location.to_record()).expect("record");
        assert_eq!(again, fx.location);
        assert!(Location::from_record("/tmp/not-a-socket").is_err());
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_after_the_connection_is_refused() {
        // Not upstream's: a crashed supervisor's socket file doesn't block the next one.
        let _forks = FORKS.read().await;
        let fx = Fixture::new();
        fx.make_dir(0o700);
        let socket = fx.location.socket_path();
        drop(stale_socket(&socket));
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(socket.exists());

        let server = fx.bind().expect("bind over the stale socket");
        assert_eq!(mode_of(server.socket_path()), 0o600);
    }

    #[tokio::test]
    async fn a_live_socket_is_taken_and_stays_untouched() {
        // Not upstream's: a second server never removes a live one's socket.
        let _forks = FORKS.read().await;
        let fx = Fixture::new();
        let _first = fx.bind().expect("first");
        let other_data = tempfile::tempdir().expect("data dir");
        let other_lock = SupervisorLock::try_acquire(other_data.path()).expect("lock");
        match Server::bind(&fx.location, &fx.identity, &other_lock) {
            Err(EndpointError::Taken(_)) => {}
            other => panic!("expected Taken, got {:?}", other.map(|_| ())),
        }
        assert!(fx.location.socket_path().exists());
        connect(&fx.location, &fx.identity)
            .await
            .expect("the first server still answers");
    }

    #[tokio::test]
    async fn a_regular_file_at_the_socket_path_is_unsafe_and_survives() {
        // Not upstream's: only a verified own socket may be removed.
        let _forks = FORKS.read().await;
        let fx = Fixture::new();
        fx.make_dir(0o700);
        let socket = fx.location.socket_path();
        fs::write(&socket, b"precious").expect("write");
        match fx.bind() {
            Err(EndpointError::Unsafe(_)) => {}
            other => panic!("expected Unsafe, got {:?}", other.map(|_| ())),
        }
        assert_eq!(fs::read(&socket).expect("read"), b"precious");
    }

    #[tokio::test]
    async fn a_directory_that_is_not_0700_is_unsafe() {
        // Not upstream's: the final directory is strictly 0700.
        let _forks = FORKS.read().await;
        for mode in [0o755, 0o770, 0o777, 0o750] {
            let fx = Fixture::new();
            fx.make_dir(mode);
            match fx.bind() {
                Err(EndpointError::Unsafe(_)) => {}
                other => panic!(
                    "mode {mode:o}: expected Unsafe, got {:?}",
                    other.map(|_| ())
                ),
            }
            match connect(&fx.location, &fx.identity).await {
                Err(EndpointError::Unsafe(_)) | Err(EndpointError::Io(_)) => {}
                other => panic!(
                    "mode {mode:o}: expected a refusal, got {:?}",
                    other.map(|_| ())
                ),
            }
        }
    }

    #[tokio::test]
    async fn a_symlink_at_the_endpoint_name_is_unsafe() {
        // Not upstream's: nothing below the runtime root may be a link.
        let _forks = FORKS.read().await;
        let fx = Fixture::new();
        let target = tempfile::tempdir().expect("target");
        fs::set_permissions(target.path(), fs::Permissions::from_mode(0o700)).expect("chmod");
        symlink(target.path(), &fx.dir).expect("symlink");
        match fx.bind() {
            Err(EndpointError::Unsafe(_)) => {}
            other => panic!("expected Unsafe, got {:?}", other.map(|_| ())),
        }
        assert_eq!(fs::read_dir(target.path()).expect("read_dir").count(), 0);
        assert!(connect(&fx.location, &fx.identity).await.is_err());
    }

    #[tokio::test]
    async fn the_client_refuses_a_socket_that_is_not_0600() {
        // Not upstream's: a looser socket mode is refused by the client.
        let _forks = FORKS.read().await;
        let fx = Fixture::new();
        fx.make_dir(0o700);
        let socket = fx.location.socket_path();
        let _listener = StdListener::bind(&socket).expect("bind");
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o666)).expect("chmod");
        match connect(&fx.location, &fx.identity).await {
            Err(EndpointError::Unsafe(_)) => {}
            other => panic!("expected Unsafe, got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn the_client_refuses_a_regular_file_where_the_socket_should_be() {
        // Not upstream's: the client checks the entry's type before connecting.
        let _forks = FORKS.read().await;
        let fx = Fixture::new();
        fx.make_dir(0o700);
        fs::write(fx.location.socket_path(), b"x").expect("write");
        assert!(connect(&fx.location, &fx.identity).await.is_err());
    }

    #[tokio::test]
    async fn connecting_with_no_server_fails() {
        // Not upstream's: nothing listening is an error, not a hang.
        let _forks = FORKS.read().await;
        let fx = Fixture::new();
        assert!(connect(&fx.location, &fx.identity).await.is_err());
    }

    #[tokio::test]
    async fn a_name_with_a_trailing_slash_is_rejected_before_anything_follows_a_link() {
        // Not upstream's: `link/` would make openat follow a symlink named `link`.
        let _forks = FORKS.read().await;
        let target = tempfile::tempdir().expect("target");
        let name = format!(
            "ofp-endpoint-test-slash-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        let link = Path::new("/tmp").join(&name);
        symlink(target.path(), &link).expect("symlink");
        let rejected = Location::new("/tmp", format!("{name}/"));
        let _ = fs::remove_file(&link);
        assert!(matches!(rejected, Err(EndpointError::Unsafe(_))));
        assert_eq!(fs::read_dir(target.path()).expect("read_dir").count(), 0);
    }

    #[tokio::test]
    async fn a_held_namespace_lock_stops_a_second_data_dir_from_touching_the_socket() {
        // Not upstream's: two supervisors of different data directories share one endpoint
        // directory. While one holds the namespace lock, the other neither probes nor removes.
        let _forks = FORKS.read().await;
        use std::os::fd::AsRawFd;
        let fx = Fixture::new();
        fx.make_dir(0o700);
        let socket = fx.location.socket_path();
        drop(stale_socket(&socket));
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).expect("chmod");

        let holder = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(fx.dir.join("supervisor.lock"))
            .expect("lock file");
        // SAFETY: the descriptor is open for the whole call.
        assert_eq!(
            unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        match fx.bind() {
            Err(EndpointError::Taken(why)) => assert!(why.contains("namespace lock"), "{why}"),
            other => panic!("expected Taken, got {:?}", other.map(|_| ())),
        }
        assert!(
            socket.exists(),
            "the stale socket was removed without the namespace lock"
        );

        drop(holder);
        let server = bind_after_release(|| fx.bind());
        assert!(server.socket_path().exists());
    }

    fn bind_after_release(mut bind: impl FnMut() -> Result<Server, EndpointError>) -> Server {
        // A parallel child's fork-to-exec window can briefly retain our flock fd.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match bind() {
                Ok(server) => return server,
                Err(EndpointError::Taken(_)) if std::time::Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Err(error) => panic!("bind after lock release: {error}"),
            }
        }
    }

    #[tokio::test]
    async fn the_namespace_lock_is_released_when_the_server_is_dropped() {
        // Not upstream's: guard cleanup, another data directory can bind right after.
        let _forks = FORKS.read().await;
        let fx = Fixture::new();
        let first = fx.bind().expect("first");
        let other_data = tempfile::tempdir().expect("data dir");
        let other_lock = SupervisorLock::try_acquire(other_data.path()).expect("lock");
        assert!(matches!(
            Server::bind(&fx.location, &fx.identity, &other_lock),
            Err(EndpointError::Taken(_))
        ));
        drop(first);
        let second = bind_after_release(|| Server::bind(&fx.location, &fx.identity, &other_lock));
        assert!(second.socket_path().exists());
    }

    #[tokio::test]
    async fn a_replacement_socket_survives_the_drop_of_the_server_it_replaced() {
        // Not upstream's: Drop unlinks only the inode it bound.
        let _forks = FORKS.read().await;
        let fx = Fixture::new();
        let server = fx.bind().expect("bind");
        let socket = server.socket_path().to_path_buf();
        fs::remove_file(&socket).expect("remove ours");
        let _replacement = StdListener::bind(&socket).expect("replacement");
        drop(server);
        assert!(socket.exists(), "the replacement was unlinked");
    }

    const OTHER_UID: u32 = 4242;
    const HELPER_ENV: &str = "OFP_ENDPOINT_TEST_SERVER_NAME";

    /// Kills and reaps the helper process however the test ends.
    struct Helper(std::process::Child);

    impl Drop for Helper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Not upstream's: the server half of the other-uid test. It does nothing unless the
    /// parent test started it (as `OTHER_UID`, with the endpoint name in the environment).
    #[tokio::test]
    async fn other_uid_helper_server() {
        let Ok(name) = std::env::var(HELPER_ENV) else {
            return;
        };
        let root = fs::canonicalize("/tmp").expect("canonical endpoint root");
        let location = Location::new(&root, name).expect("location");
        assert!(
            location.socket_path().as_os_str().len() <= 103,
            "SET-UP: canonical socket path exceeds 103 bytes: {}",
            location.socket_path().display()
        );
        let data = tempfile::tempdir().expect("data dir");
        let lock = SupervisorLock::try_acquire(data.path()).expect("lock");
        let identity = Identity::current(ServiceKind::User).expect("identity");
        let mut server = Server::bind(&location, &identity, &lock).expect("bind");
        // libtest prints `test <name> ... ` first, so this lands mid-line: the parent looks for
        // the token anywhere in a line.
        println!("OFP-HELPER-READY");
        // A peer of another uid is dropped inside accept, so this only ever times out.
        let outcome =
            tokio::time::timeout(std::time::Duration::from_secs(3), server.accept()).await;
        println!(
            "OFP-HELPER-SUMMARY accepted={} rejected={}",
            outcome.is_ok(),
            server.rejected_peers()
        );
    }

    #[tokio::test]
    async fn a_peer_of_another_uid_is_dropped_by_the_server() {
        // Not upstream's: needs root to start the server as another user; skipped otherwise.
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: not root");
            return;
        }
        let fx = Fixture::new();
        let name = fx
            .location
            .dir()
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        // Exclusive, so no other test's descriptor is open in the child.
        let fork = FORKS.write().await;
        let child = Command::new(exe)
            .args([
                "--exact",
                "unix::other_uid_helper_server",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_ENV, &name)
            .uid(OTHER_UID)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        drop(fork);
        let mut helper = match child {
            Ok(child) => Helper(child),
            Err(e) => {
                eprintln!("skipped: the test binary can't run as uid {OTHER_UID}: {e}");
                return;
            }
        };
        let stdout = helper.0.stdout.take().expect("stdout");
        let mut lines = BufReader::new(stdout).lines();
        let mut ready = false;
        let mut seen = Vec::new();
        while let Some(Ok(line)) = lines.next() {
            if line.contains("OFP-HELPER-READY") {
                ready = true;
                break;
            }
            seen.push(line);
        }
        assert!(
            ready,
            "the helper server never came up; its stdout was {seen:?}"
        );

        // We are root; the server wants OTHER_UID. The client side sees the server's uid is
        // right, and the server drops us.
        let identity = Identity::for_uid(ServiceKind::User, OTHER_UID);
        let mut client = connect(&fx.location, &identity).await.expect("connect");
        let mut buf = [0u8; 1];
        let read =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.read(&mut buf)).await;
        assert!(
            matches!(read, Ok(Ok(0)) | Ok(Err(_))),
            "the server kept a peer of another uid: {read:?}"
        );

        let summary = lines.find_map(|line| {
            let line = line.ok()?;
            line.split_once("OFP-HELPER-SUMMARY ")
                .map(|(_, rest)| rest.trim().to_string())
        });
        assert_eq!(summary.as_deref(), Some("accepted=false rejected=1"));
    }

    #[tokio::test]
    async fn a_client_refuses_a_server_whose_actual_uid_differs_though_the_files_look_right() {
        // Not upstream's: needs root. The directory and socket are handed to OTHER_UID so the
        // metadata checks pass, and only the peer-credential check can refuse the root server.
        let _forks = FORKS.read().await;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: not root");
            return;
        }
        let fx = Fixture::new();
        let server = fx.bind().expect("bind as root");
        std::os::unix::fs::chown(&fx.dir, Some(OTHER_UID), None).expect("chown dir");
        std::os::unix::fs::chown(server.socket_path(), Some(OTHER_UID), None)
            .expect("chown socket");
        assert_eq!(mode_of(&fx.dir), 0o700);
        assert_eq!(mode_of(server.socket_path()), 0o600);

        let identity = Identity::for_uid(ServiceKind::User, OTHER_UID);
        match connect(&fx.location, &identity).await {
            Err(EndpointError::PeerRejected(why)) => {
                assert!(why.contains("runs as uid 0"), "{why}")
            }
            other => panic!("expected PeerRejected, got {:?}", other.map(|_| ())),
        }
        drop(server);
    }

    #[test]
    fn a_location_needs_an_absolute_root_and_one_name() {
        // Not upstream's: Location::new's shape check.
        assert!(Location::new("relative", "x").is_err());
        assert!(Location::new("/tmp", "a/b").is_err());
        assert!(Location::new("/tmp", "..").is_err());
        assert!(Location::new("/tmp", "").is_err());
        assert!(Location::new("/tmp", "ok").is_ok());
    }
}

#[cfg(windows)]
mod windows {
    use open_ferry_supervisor::endpoint::{
        EndpointError, Identity, Location, Server, ServiceKind, connect, pipe_client_pid,
        pipe_server_pid, verify_peer_pid,
    };
    use open_ferry_supervisor::process::SupervisorLock;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Fixture {
        _data: tempfile::TempDir,
        lock: SupervisorLock,
        location: Location,
        identity: Identity,
    }

    fn fixture() -> Fixture {
        let data = tempfile::tempdir().expect("data dir");
        let lock = SupervisorLock::try_acquire(data.path()).expect("lock");
        let location = Location::resolve(
            &Identity::current(ServiceKind::User).expect("identity"),
            data.path(),
        )
        .expect("location");
        Fixture {
            _data: data,
            lock,
            location,
            identity: Identity::current(ServiceKind::User).expect("identity"),
        }
    }

    #[test]
    fn the_pipe_name_has_the_documented_shape_and_follows_the_directory() {
        // Not upstream's: \\.\pipe\open-ferry-supervisor-<16 hex>.
        let a = fixture();
        let name = a.location.pipe_name();
        let tail = name
            .strip_prefix(r"\\.\pipe\open-ferry-supervisor-")
            .expect("prefix");
        assert_eq!(tail.len(), 16);
        assert!(
            tail.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        let b = fixture();
        assert_ne!(a.location, b.location);
        assert_eq!(
            Location::from_record(&a.location.to_record()).expect("record"),
            a.location
        );
    }

    #[tokio::test]
    async fn a_client_and_server_talk_and_check_each_other() {
        // Not upstream's: a round trip, the client's SID check, and the pid helpers.
        let fx = fixture();
        let mut server = Server::bind(&fx.location, &fx.identity, &fx.lock).expect("bind");
        let accept = tokio::spawn(async move {
            let mut connection = server.accept().await.expect("accept");
            let mut buf = [0u8; 4];
            connection.stream.read_exact(&mut buf).await.expect("read");
            connection.stream.write_all(&buf).await.expect("write");
            let pid = pipe_client_pid(&connection.stream).expect("client pid");
            (connection.peer, pid)
        });
        let mut client = connect(&fx.location, &fx.identity).await.expect("connect");
        client.write_all(b"ping").await.expect("write");
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.expect("read");
        assert_eq!(&echoed, b"ping");

        let (peer, client_pid) = accept.await.expect("task");
        assert_eq!(peer.pid, Some(std::process::id()));
        assert_eq!(client_pid, std::process::id());
        assert_eq!(
            pipe_server_pid(&client).expect("server pid"),
            std::process::id()
        );
    }

    #[tokio::test]
    async fn the_server_pid_check_accepts_the_right_process_and_refuses_another() {
        // Not upstream's: the control pipe's optional peer verification.
        let fx = fixture();
        let mut server = Server::bind(&fx.location, &fx.identity, &fx.lock).expect("bind");
        let _accept = tokio::spawn(async move { server.accept().await.map(|_| ()) });
        let client = connect(&fx.location, &fx.identity).await.expect("connect");
        let me = std::process::id();
        verify_peer_pid(
            &client,
            open_ferry_supervisor::endpoint::PeerEnd::Server,
            me,
        )
        .expect("same process");
        let err = verify_peer_pid(
            &client,
            open_ferry_supervisor::endpoint::PeerEnd::Server,
            me.wrapping_add(4),
        )
        .expect_err("another process");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn a_second_server_on_the_same_name_is_a_startup_error() {
        // Not upstream's: FILE_FLAG_FIRST_PIPE_INSTANCE.
        let fx = fixture();
        let _first = Server::bind(&fx.location, &fx.identity, &fx.lock).expect("first");
        match Server::bind(&fx.location, &fx.identity, &fx.lock) {
            Err(EndpointError::Taken(_)) => {}
            other => panic!("expected Taken, got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn a_client_refuses_a_server_owned_by_another_sid() {
        // Not upstream's: the owner SID of the serving process is checked.
        let fx = fixture();
        let mut server = Server::bind(&fx.location, &fx.identity, &fx.lock).expect("bind");
        let _accept = tokio::spawn(async move { server.accept().await.map(|_| ()) });
        let stranger = Identity::for_sid(ServiceKind::User, "S-1-5-21-1-2-3-9999");
        match connect(&fx.location, &stranger).await {
            Err(EndpointError::PeerRejected(_)) => {}
            other => panic!("expected PeerRejected, got {:?}", other.map(|_| ())),
        }
    }
}
