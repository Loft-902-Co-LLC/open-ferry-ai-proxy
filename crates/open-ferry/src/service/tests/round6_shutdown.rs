use super::*;
use futures_util::FutureExt;

#[tokio::test]
async fn round6_sync_close_answers_a_preclose_permit() {
    let dir = tempfile::tempdir().unwrap();
    let mut service = service(dir.path(), "");
    let sync = sync_sender(&mut service);
    let permit = sync.requests.reserve().await.unwrap();
    let gate = service.gate.clone();
    let (applied, done) = oneshot::channel();
    let request = SyncRequest {
        change: SyncChange::FileRemoved(dir.path().join("late.json")),
        revision: next_revision(),
        applied,
        operation: Some(gate.begin_operation().unwrap()),
    };
    {
        let close = service.close_sync();
        tokio::pin!(close);
        let polled =
            std::future::poll_fn(|cx| std::task::Poll::Ready(close.as_mut().poll(cx))).await;
        assert!(
            polled.is_pending(),
            "close ignored an outstanding sync permit"
        );
        permit.send(request);
        close.await;
    }
    assert_eq!(
        done.await.map_err(|_| SyncError::Stopped),
        Err(SyncError::Stopped)
    );
    assert!(service.sync_requests.is_closed());
    assert!(service.sync_requests.recv().await.is_none());
    assert!(
        gate.close(gate.epoch()).now_or_never().is_some(),
        "late sync retained a gate operation"
    );
}

#[tokio::test]
async fn round6_reload_close_answers_a_preclose_permit() {
    let dir = tempfile::tempdir().unwrap();
    let mut service = service(dir.path(), "");
    let (requests, receiver) = mpsc::channel(1);
    service.reload_requests = receiver;
    let reload = ReloadSender { requests };
    let permit = reload.requests.reserve().await.unwrap();
    let gate = service.gate.clone();
    let (applied, done) = oneshot::channel();
    let request = ReloadRequest {
        force: false,
        applied,
        operation: Some(gate.begin_operation().unwrap()),
    };
    {
        let close = service.close_reloads();
        tokio::pin!(close);
        let polled =
            std::future::poll_fn(|cx| std::task::Poll::Ready(close.as_mut().poll(cx))).await;
        assert!(
            polled.is_pending(),
            "close ignored an outstanding reload permit"
        );
        permit.send(request);
        close.await;
    }
    assert_eq!(done.await, Ok(()));
    assert!(service.reload_requests.is_closed());
    assert!(service.reload_requests.recv().await.is_none());
    assert!(service.pending_reloads.is_empty());
    assert!(
        gate.close(gate.epoch()).now_or_never().is_some(),
        "late reload retained a gate operation"
    );
}
