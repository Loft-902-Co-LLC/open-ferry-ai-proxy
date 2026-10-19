//! Lock-free endpoint access for supervised workers and update callers.

use std::path::{Path, PathBuf};
use std::pin::Pin as TaskPin;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use std::time::Instant;

use open_ferry_update::fetch::BoxFuture;
use open_ferry_update::journal::Artifact;
use open_ferry_update::pins::Pin;
use open_ferry_update::{
    CancelOutcome, SwitchError, SwitchEvent, SwitchHandle, SwitchProgress, Switcher,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};

use super::{locate, time_left, within};
use crate::endpoint::{ClientStream, EndpointError, Identity, Location, connect};
use crate::protocol::{Admission, Channel, EndpointReply, EndpointRequest, How, VERSION};
use crate::snapshot::Snapshot;

type EndpointChannel = Channel<ReadHalf<ClientStream>, CountingWriter<WriteHalf<ClientStream>>>;

/// Count bytes the transport accepted, including a named pipe's own buffer.
/// A pending or failed first write alone does not make a request uncertain.
pub(super) struct CountingWriter<W> {
    writer: W,
    accepted: Arc<AtomicUsize>,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for CountingWriter<W> {
    fn poll_write(
        mut self: TaskPin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = TaskPin::new(&mut self.writer).poll_write(cx, bytes);
        if let Poll::Ready(Ok(count)) = result {
            self.accepted.fetch_add(count, Ordering::Relaxed);
        }
        result
    }
    fn poll_flush(mut self: TaskPin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        TaskPin::new(&mut self.writer).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: TaskPin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        TaskPin::new(&mut self.writer).poll_shutdown(cx)
    }
}

/// A switch request. Admission is always stamped by the supervisor itself.
#[derive(Clone, Debug)]
pub struct SwitchRequest {
    pub to: String,
    pub sha256: String,
    /// A live staged pin for an update; empty for a rollback.
    pub pin: String,
    pub how: How,
    pub auto: bool,
}

/// Read the full typed status without probing or taking either lifetime lock.
pub async fn status(
    data_dir: &Path,
    identity: &Identity,
    deadline: Instant,
) -> Result<Snapshot, SwitchError> {
    let mut channel = request(
        data_dir,
        identity,
        EndpointRequest::Status { v: VERSION },
        deadline,
        |location| dial(location, identity),
    )
    .await?;
    match receive(&mut channel, deadline).await? {
        EndpointReply::Status { snapshot, .. } => serde_json::from_value(snapshot)
            .map_err(|error| SwitchError::Protocol(format!("invalid status snapshot: {error}"))),
        other => Err(unexpected(other)),
    }
}

/// Send a switch and return at `Accepted`. A refusal is an error. The
/// returned handle owns this connection, without a background reader or any
/// cancellation on drop. A timeout can leave a request running: read status.
pub async fn switch(
    data_dir: &Path,
    identity: &Identity,
    request_switch: SwitchRequest,
    deadline: Instant,
) -> Result<SwitchHandle, SwitchError> {
    let request_switch = EndpointRequest::Switch {
        v: VERSION,
        to: request_switch.to,
        sha256: request_switch.sha256,
        pin: request_switch.pin,
        how: request_switch.how,
        auto: request_switch.auto,
        admitted: Admission {
            while_off: false,
            mode_gen: 0,
        },
    };
    let mut channel = request(data_dir, identity, request_switch, deadline, |location| {
        dial(location, identity)
    })
    .await?;
    match receive(&mut channel, deadline).await? {
        EndpointReply::Accepted { id, .. } if !id.is_empty() => Ok(SwitchHandle::new(
            id.clone(),
            Box::new(Progress { id, channel }),
        )),
        other => Err(unexpected(other)),
    }
}

/// Cancel the current switch, or report that it is too late or none exists.
pub async fn cancel(
    data_dir: &Path,
    identity: &Identity,
    deadline: Instant,
) -> Result<CancelOutcome, SwitchError> {
    let mut channel = request(
        data_dir,
        identity,
        EndpointRequest::Cancel { v: VERSION },
        deadline,
        |location| dial(location, identity),
    )
    .await?;
    match receive(&mut channel, deadline).await {
        Ok(EndpointReply::Cancelled { .. }) => Ok(CancelOutcome::Cancelled),
        Ok(EndpointReply::TooLate { .. }) => Ok(CancelOutcome::TooLate),
        Err(SwitchError::Refused(reason)) if reason == "nothing is switching" => {
            Ok(CancelOutcome::Nothing)
        }
        Ok(other) => Err(unexpected(other)),
        Err(error) => Err(error),
    }
}

async fn dial(
    location: Location,
    identity: &Identity,
) -> Result<(ReadHalf<ClientStream>, WriteHalf<ClientStream>), SwitchError> {
    let stream = connect(&location, identity).await.map_err(endpoint_error)?;
    stream
        .writable()
        .await
        .map_err(|error| SwitchError::Unreachable(error.to_string()))?;
    #[cfg(test)]
    super::tests::after_writable();
    Ok(tokio::io::split(stream))
}

pub(super) async fn request<R, W, F>(
    data_dir: &Path,
    identity: &Identity,
    request: EndpointRequest,
    deadline: Instant,
    dial: impl FnOnce(Location) -> F,
) -> Result<Channel<R, CountingWriter<W>>, SwitchError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: std::future::Future<Output = Result<(R, W), SwitchError>>,
{
    let encoded = serde_json::to_vec(&request).map_err(|_| {
        SwitchError::Unreachable("could not encode the request before sending".into())
    })?;
    if encoded.len() >= crate::protocol::MAX_FRAME {
        return Err(SwitchError::Unreachable(
            "could not encode the request before sending".into(),
        ));
    }
    let (dir, owner) = (data_dir.to_owned(), identity.clone());
    let location = within(deadline, move || locate(&dir, &owner))
        .await
        .ok_or_else(before_send_timeout)?
        .map_err(SwitchError::Untrusted)?;
    let (reader, writer) =
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), dial(location))
            .await
            .map_err(|_| before_send_timeout())??;
    if time_left(deadline).is_zero() {
        return Err(before_send_timeout());
    }
    let accepted = Arc::new(AtomicUsize::new(0));
    let mut channel = Channel::new(
        reader,
        CountingWriter {
            writer,
            accepted: accepted.clone(),
        },
    );
    let sent = tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        send(&mut channel, &request),
    )
    .await
    .map_err(|_| SwitchError::TimedOut)
    .and_then(|sent| sent);
    if let Err(error) = sent {
        return Err(if accepted.load(Ordering::Relaxed) == 0 {
            match error {
                SwitchError::TimedOut => before_send_timeout(),
                _ => SwitchError::Unreachable("the connection failed before sending".into()),
            }
        } else {
            error
        });
    }
    Ok(channel)
}

fn before_send_timeout() -> SwitchError {
    SwitchError::Unreachable("timed out before sending".into())
}

fn endpoint_error(error: EndpointError) -> SwitchError {
    match error {
        EndpointError::Io(error) => SwitchError::Unreachable(error.to_string()),
        EndpointError::SwitchingDisabled(why) => SwitchError::Disabled(why),
        other => SwitchError::Untrusted(other.to_string()),
    }
}

async fn bounded<T>(
    deadline: Instant,
    work: impl std::future::Future<Output = Result<T, SwitchError>>,
) -> Result<T, SwitchError> {
    if time_left(deadline).is_zero() {
        return Err(SwitchError::TimedOut);
    }
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), work)
        .await
        .map_err(|_| SwitchError::TimedOut)?
}

async fn send<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    channel: &mut Channel<R, W>,
    request: &EndpointRequest,
) -> Result<(), SwitchError> {
    channel
        .send(request)
        .await
        .map_err(|_| SwitchError::Stopped)
}

async fn receive<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    channel: &mut Channel<R, W>,
    deadline: Instant,
) -> Result<EndpointReply, SwitchError> {
    let reply = bounded(deadline, async {
        channel
            .receive::<EndpointReply>()
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::InvalidData => SwitchError::Protocol(error.to_string()),
                _ => SwitchError::Stopped,
            })?
            .map(|frame| frame.message)
            .ok_or(SwitchError::Stopped)
    })
    .await?;
    let v = match &reply {
        EndpointReply::Status { v, .. }
        | EndpointReply::KeepSet { v, .. }
        | EndpointReply::Accepted { v, .. }
        | EndpointReply::Progress { v, .. }
        | EndpointReply::Done { v, .. }
        | EndpointReply::Cancelled { v }
        | EndpointReply::TooLate { v }
        | EndpointReply::Stopping { v }
        | EndpointReply::Error { v, .. }
        | EndpointReply::Unsupported { v, .. } => *v,
        EndpointReply::Unknown => return Err(unexpected(reply)),
    };
    if v != VERSION {
        return Err(SwitchError::Protocol(format!(
            "protocol version {v}, expected {VERSION}"
        )));
    }
    if let EndpointReply::Error { reason, .. } = reply {
        return Err(SwitchError::Refused(reason));
    }
    if let EndpointReply::Unsupported { message_type, .. } = reply {
        return Err(SwitchError::Protocol(format!(
            "the supervisor does not support {message_type} requests"
        )));
    }
    Ok(reply)
}

fn unexpected(reply: EndpointReply) -> SwitchError {
    SwitchError::Protocol(format!("unexpected reply: {reply:?}"))
}

struct Progress {
    id: String,
    channel: EndpointChannel,
}

impl SwitchProgress for Progress {
    fn next(&mut self, deadline: Instant) -> BoxFuture<'_, Result<SwitchEvent, SwitchError>> {
        Box::pin(async move {
            let reply =
                receive(&mut self.channel, deadline)
                    .await
                    .map_err(|error| match error {
                        SwitchError::Refused(reason) => SwitchError::Ended(reason),
                        other => other,
                    })?;
            match reply {
                EndpointReply::Progress { id, phase, .. } if id == self.id => {
                    Ok(SwitchEvent::Progress { phase })
                }
                EndpointReply::Done { id, result, .. } if id == self.id => {
                    Ok(SwitchEvent::Done { result })
                }
                other => Err(unexpected(other)),
            }
        })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use super::*;

    struct ReadError;

    impl AsyncRead for ReadError {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("read failed")))
        }
    }

    pub(in crate::client) struct PartialWriteError(pub(in crate::client) Arc<AtomicUsize>);

    impl AsyncWrite for PartialWriteError {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Poll::Ready(Ok(1))
            } else {
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "send failed",
                )))
            }
        }

        fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    pub(in crate::client) enum Failure {
        FirstWrite,
        PendingWrite,
        Flush,
    }
    pub(in crate::client) struct FailingWriter {
        pub(in crate::client) failure: Failure,
        pub(in crate::client) polls: Arc<AtomicUsize>,
    }
    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            match self.failure {
                Failure::FirstWrite => Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe))),
                Failure::PendingWrite => Poll::Pending,
                Failure::Flush => Poll::Ready(Ok(bytes.len())),
            }
        }
        fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::from(io::ErrorKind::InvalidData)))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    fn switch_request() -> EndpointRequest {
        EndpointRequest::Switch {
            v: VERSION,
            to: "2.0.0".into(),
            sha256: "ab".repeat(32),
            pin: "pin".into(),
            how: How::Update,
            auto: false,
            admitted: Admission {
                while_off: false,
                mode_gen: 0,
            },
        }
    }

    #[tokio::test]
    async fn a_send_error_after_a_partial_request_is_ambiguous() {
        let writes = Arc::new(AtomicUsize::new(0));
        let mut channel = Channel::new(tokio::io::empty(), PartialWriteError(writes.clone()));
        assert!(matches!(
            send(&mut channel, &switch_request()).await,
            Err(SwitchError::Stopped)
        ));
        assert_eq!(writes.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_read_error_after_sending_is_ambiguous() {
        use tokio::io::AsyncReadExt;

        let (writer, mut peer) = tokio::io::duplex(4096);
        let mut channel = Channel::new(ReadError, writer);
        send(&mut channel, &switch_request()).await.unwrap();
        let mut bytes = [0; 4096];
        let count = peer.read(&mut bytes).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes[..count]).unwrap()["type"],
            "switch"
        );
        assert!(matches!(
            receive(
                &mut channel,
                Instant::now() + std::time::Duration::from_secs(1)
            )
            .await,
            Err(SwitchError::Stopped)
        ));
    }

    #[test]
    fn disabled_switching_has_its_own_error_and_keeps_the_policy_reason() {
        let error = endpoint_error(EndpointError::SwitchingDisabled(
            "no private runtime directory".into(),
        ));
        assert!(
            matches!(&error, SwitchError::Disabled(reason) if reason == "no private runtime directory")
        );
        assert_eq!(
            error.to_string(),
            "switching is disabled: no private runtime directory"
        );
    }
}

/// A switcher for the data directory named in the worker's authenticated
/// welcome (or a CLI's authorized update home). Construction has no I/O.
/// Each call reads that directory's endpoint record and checks its peer.
#[derive(Clone, Debug)]
pub struct EndpointSwitcher {
    data_dir: PathBuf,
    identity: Identity,
}

impl EndpointSwitcher {
    pub fn new(data_dir: impl Into<PathBuf>, identity: Identity) -> Self {
        Self {
            data_dir: data_dir.into(),
            identity,
        }
    }
}

impl Switcher for EndpointSwitcher {
    fn status(&self, deadline: Instant) -> BoxFuture<'_, Result<serde_json::Value, SwitchError>> {
        Box::pin(async move {
            serde_json::to_value(status(&self.data_dir, &self.identity, deadline).await?)
                .map_err(|error| SwitchError::Protocol(error.to_string()))
        })
    }

    fn update<'a>(
        &'a self,
        pin: &'a Pin,
        auto: bool,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<SwitchHandle, SwitchError>> {
        Box::pin(switch(
            &self.data_dir,
            &self.identity,
            SwitchRequest {
                to: pin.version.clone(),
                sha256: pin.sha256.clone(),
                pin: pin.id.clone(),
                how: How::Update,
                auto,
            },
            deadline,
        ))
    }

    fn rollback<'a>(
        &'a self,
        target: &'a Artifact,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<SwitchHandle, SwitchError>> {
        Box::pin(switch(
            &self.data_dir,
            &self.identity,
            SwitchRequest {
                to: target.version.clone(),
                sha256: target.sha256.clone(),
                pin: String::new(),
                how: How::Rollback,
                auto: false,
            },
            deadline,
        ))
    }

    fn cancel(&self, deadline: Instant) -> BoxFuture<'_, Result<CancelOutcome, SwitchError>> {
        Box::pin(cancel(&self.data_dir, &self.identity, deadline))
    }
}
