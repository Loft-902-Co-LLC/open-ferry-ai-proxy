#![cfg(unix)]
use open_ferry_supervisor::{
    process::unix::{WorkerProcess, process_alive},
    protocol::{Channel, Message},
};
use std::{io, net::TcpListener, process::Command, time::Duration};
use tokio::io::AsyncReadExt;

type Control = Channel<tokio::net::unix::OwnedReadHalf, tokio::net::unix::OwnedWriteHalf>;
fn spawn(listener: &TcpListener, dir: &std::path::Path, marker: &str) -> (WorkerProcess, Control) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_unix_probe"));
    command.arg(dir).arg(marker);
    let (worker, control) =
        WorkerProcess::spawn(command, &[listener.try_clone().unwrap()]).unwrap();
    let control = tokio::net::UnixStream::from_std(control).unwrap();
    let (read, write) = control.into_split();
    (worker, Channel::new(read, write))
}
async fn receive(channel: &mut Control) -> Message {
    tokio::time::timeout(Duration::from_secs(5), channel.receive::<Message>())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .message
}
async fn reply(mut stream: tokio::net::TcpStream) -> io::Result<String> {
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes)).await??;
    String::from_utf8(bytes).map_err(io::Error::other)
}
// Not upstream's: real listeners share one backlog across two processes; pause
// drops the mio listener and resume rebuilds it, including already-queued clients.
#[tokio::test]
async fn passing_pause_resume_and_inheritance() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (mut a, mut ac) = spawn(&listener, dir_a.path(), "A");
    let (mut b, mut bc) = spawn(&listener, dir_b.path(), "B");
    assert!(matches!(receive(&mut ac).await, Message::Loaded { .. }));
    assert!(matches!(receive(&mut bc).await, Message::Loaded { .. }));
    let identity = a.identity();
    assert!(process_alive(identity.pid, identity.started));
    assert!(!process_alive(
        identity.pid,
        identity.started.wrapping_add(1)
    ));
    ac.send(&Message::TakeOver).await.unwrap();
    assert_eq!(receive(&mut ac).await, Message::Ready);
    for _ in 0..20 {
        assert_eq!(
            reply(tokio::net::TcpStream::connect(address).await.unwrap())
                .await
                .unwrap(),
            "A"
        );
        ac.send(&Message::HandOver { budget_ms: 100 })
            .await
            .unwrap();
        assert!(matches!(receive(&mut ac).await, Message::HandedOver { .. }));
        let queued = tokio::net::TcpStream::connect(address).await.unwrap();
        ac.send(&Message::Resume).await.unwrap();
        assert_eq!(receive(&mut ac).await, Message::Ready);
        assert_eq!(reply(queued).await.unwrap(), "A");
    }
    ac.send(&Message::HandOver { budget_ms: 100 })
        .await
        .unwrap();
    assert!(matches!(receive(&mut ac).await, Message::HandedOver { .. }));
    bc.send(&Message::TakeOver).await.unwrap();
    assert_eq!(receive(&mut bc).await, Message::Ready);
    for _ in 0..20 {
        assert_eq!(
            reply(tokio::net::TcpStream::connect(address).await.unwrap())
                .await
                .unwrap(),
            "B"
        );
    }
    ac.send(&Message::Stop { budget_ms: 100 }).await.unwrap();
    bc.send(&Message::Stop { budget_ms: 100 }).await.unwrap();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    assert!(!process_alive(identity.pid, identity.started));
}
// Not upstream's: real exec with source sockets already at fd 3 and 4 must not
// clobber control or either listener while mapping the fd 3,4,5 targets.
#[test]
fn descriptor_collisions() {
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    for mode in ["--collision", "--closed-stdio-spawn-error"] {
        let mut child = OwnedChild(
            Command::new(env!("CARGO_BIN_EXE_unix_probe"))
                .arg(mode)
                .arg(dir.path())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success(), "{mode}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "collision probe timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
// Not upstream's: control EOF and owner-drop both reap only the owned worker.
#[tokio::test]
async fn eof_and_drop_cleanup() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (mut worker, mut channel) = spawn(&listener, dir.path(), "A");
    assert!(matches!(
        receive(&mut channel).await,
        Message::Loaded { .. }
    ));
    let identity = worker.identity();
    drop(channel);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = worker.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "worker did not stop on EOF"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!process_alive(identity.pid, identity.started));
    let (worker, _channel) = spawn(&listener, dir.path(), "B");
    let identity = worker.identity();
    drop(worker);
    assert!(!process_alive(identity.pid, identity.started));
    let (mut worker, mut channel) = spawn(&listener, dir.path(), "C");
    assert!(matches!(
        receive(&mut channel).await,
        Message::Loaded { .. }
    ));
    assert!(worker.wait_timeout(Duration::ZERO).unwrap().is_none());
    worker.kill().unwrap();
    assert!(worker.try_wait().unwrap().is_some());
}
