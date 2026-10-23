use super::*;
use std::sync::atomic::AtomicUsize;
use std::task::Wake;

struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

struct PendingIo;

impl AsyncRead for PendingIo {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

impl AsyncWrite for PendingIo {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Pending
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

fn poll_io(
    io: &mut Tracked<PendingIo>,
    cx: &mut Context<'_>,
    operation: usize,
) -> Poll<io::Result<()>> {
    let io = Pin::new(io);
    match operation {
        0 => io.poll_read(cx, &mut ReadBuf::new(&mut [0; 1])),
        1 => io.poll_write(cx, b"x").map_ok(|_| ()),
        2 => io
            .poll_write_vectored(cx, &[io::IoSlice::new(b"x")])
            .map_ok(|_| ()),
        3 => io.poll_flush(cx),
        4 => io.poll_shutdown(cx),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn cut_wakes_cached_and_replaced_wakers_for_every_io_operation() {
    for operation in 0..5 {
        for replace in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (client, server) = tokio::join!(
                TcpStream::connect(listener.local_addr().unwrap()),
                listener.accept()
            );
            let (_client, (server, _)) = (client.unwrap(), server.unwrap());
            let control = ServeControl::new();
            let core = ConnCore::register(&control.shared, &server).unwrap();
            let mut io = Tracked {
                inner: PendingIo,
                core,
                wakers: [None, None],
            };
            let old = Arc::new(WakeCount(AtomicUsize::new(0)));
            let new = Arc::new(WakeCount(AtomicUsize::new(0)));
            let old_waker = Waker::from(old.clone());
            let new_waker = Waker::from(new.clone());
            let mut old_cx = Context::from_waker(&old_waker);
            assert!(poll_io(&mut io, &mut old_cx, operation).is_pending());
            assert!(poll_io(&mut io, &mut old_cx, operation).is_pending());
            let mut cx = Context::from_waker(if replace { &new_waker } else { &old_waker });
            assert!(poll_io(&mut io, &mut cx, operation).is_pending());
            assert_eq!(control.cut(), 1);
            assert_eq!(control.live_count(), 0);
            assert_eq!(old.0.load(Ordering::Relaxed), usize::from(!replace));
            assert_eq!(new.0.load(Ordering::Relaxed), usize::from(replace));
            let Poll::Ready(Err(error)) = poll_io(&mut io, &mut cx, operation) else {
                panic!("cut left operation {operation} pending");
            };
            assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
            assert_eq!(error.to_string(), "the connection was cut");
        }
    }
}
