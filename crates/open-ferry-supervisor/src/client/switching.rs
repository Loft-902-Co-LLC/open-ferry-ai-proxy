//! Lock-free endpoint access for supervised workers and update callers.

use std::path::{Path, PathBuf};
use std::time::Instant;

use open_ferry_update::fetch::BoxFuture;
use open_ferry_update::journal::Artifact;
use open_ferry_update::pins::Pin;
use open_ferry_update::{
    CancelOutcome, SwitchError, SwitchEvent, SwitchHandle, SwitchProgress, Switcher,
};
use tokio::io::{ReadHalf, WriteHalf};

use super::{locate, time_left, within};
use crate::endpoint::{ClientStream, EndpointError, Identity, connect};
use crate::protocol::{Admission, Channel, EndpointReply, EndpointRequest, How, VERSION};
use crate::snapshot::Snapshot;

type EndpointChannel = Channel<ReadHalf<ClientStream>, WriteHalf<ClientStream>>;

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
    let mut channel = request(data_dir, identity, request_switch, deadline).await?;
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

async fn request(
    data_dir: &Path,
    identity: &Identity,
    request: EndpointRequest,
    deadline: Instant,
) -> Result<EndpointChannel, SwitchError> {
    let (dir, owner) = (data_dir.to_owned(), identity.clone());
    let location = within(deadline, move || locate(&dir, &owner))
        .await
        .ok_or(SwitchError::TimedOut)?
        .map_err(SwitchError::Untrusted)?;
    bounded(deadline, async {
        let stream = connect(&location, identity).await.map_err(endpoint_error)?;
        stream.writable().await.map_err(io_error)?;
        #[cfg(test)]
        super::tests::after_writable();
        if time_left(deadline).is_zero() {
            return Err(SwitchError::TimedOut);
        }
        let (reader, writer) = tokio::io::split(stream);
        let mut channel = Channel::new(reader, writer);
        channel.send(&request).await.map_err(io_error)?;
        Ok(channel)
    })
    .await
}

fn endpoint_error(error: EndpointError) -> SwitchError {
    match error {
        EndpointError::Io(error) => SwitchError::Unreachable(error.to_string()),
        EndpointError::SwitchingDisabled(why) => {
            SwitchError::Unreachable(format!("switching is disabled: {why}"))
        }
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

fn io_error(error: std::io::Error) -> SwitchError {
    if error.kind() == std::io::ErrorKind::InvalidData {
        SwitchError::Protocol(error.to_string())
    } else {
        SwitchError::Unreachable(error.to_string())
    }
}

async fn receive(
    channel: &mut EndpointChannel,
    deadline: Instant,
) -> Result<EndpointReply, SwitchError> {
    let reply = bounded(deadline, async {
        channel
            .receive::<EndpointReply>()
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe => SwitchError::Stopped,
                _ => io_error(error),
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
mod tests {
    use super::*;

    #[test]
    fn disabled_switching_is_unavailable_and_keeps_the_policy_reason() {
        let error = endpoint_error(EndpointError::SwitchingDisabled(
            "no private runtime directory".into(),
        ));
        assert!(
            matches!(&error, SwitchError::Unreachable(reason) if reason == "switching is disabled: no private runtime directory")
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
