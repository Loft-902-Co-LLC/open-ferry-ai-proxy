//! Not upstream's: the endpoint's request handlers (design v3 8.1).
//!
//! One request per connection, newline-delimited JSON as everywhere else
//! ([`Channel`]). `status`, `keep_set` and `cancel` answer from the snapshot
//! the core publishes and never wait for the core, so they stay quick while a
//! switch holds it. `switch` hands the request to the core and streams its
//! replies (`accepted`, `progress`, `done` or `error`) until the last. `stop`
//! asks the core to stop. Handlers are generic over the stream so tests run
//! them on `tokio::io::duplex`; [`accept_loop`] is the one place that touches
//! the real server.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::core::{Command, SwitchRequest};
use crate::endpoint::Server;
use crate::protocol::{Channel, EndpointErrorCode, EndpointReply, EndpointRequest, Frame, VERSION};
use crate::snapshot::{CancelAnswer, Shared};

/// How long a client has to send its request line.
pub const REQUEST_WAIT: Duration = Duration::from_secs(10);

/// What a handler needs from the supervisor.
#[derive(Clone)]
pub struct Serve {
    pub commands: UnboundedSender<Command>,
    pub shared: Arc<Shared>,
}

impl Serve {
    pub fn new(commands: UnboundedSender<Command>, shared: Arc<Shared>) -> Self {
        Self { commands, shared }
    }
}

fn error(reason: &str) -> EndpointReply {
    refusal(None, reason)
}

fn refusal(code: Option<EndpointErrorCode>, reason: &str) -> EndpointReply {
    EndpointReply::Error {
        v: VERSION,
        code,
        reason: reason.to_owned(),
    }
}

/// Serves one connection: reads the request, answers it, returns.
pub async fn handle<S>(stream: S, serve: &Serve)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, writer) = tokio::io::split(stream);
    let mut channel = Channel::new(reader, writer);
    let received = tokio::time::timeout(REQUEST_WAIT, channel.receive::<EndpointRequest>()).await;
    let frame = match received {
        Ok(Ok(Some(frame))) => frame,
        Ok(Ok(None)) => return,
        Ok(Err(why)) => {
            tracing::debug!("bad endpoint request: {why}");
            let _ = channel.send(&error("bad request")).await;
            return;
        }
        Err(_) => {
            let _ = channel.send(&error("no request was sent in time")).await;
            return;
        }
    };
    answer(&mut channel, serve, frame).await;
}

async fn answer<R, W>(channel: &mut Channel<R, W>, serve: &Serve, frame: Frame<EndpointRequest>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let version = match &frame.message {
        EndpointRequest::Status { v }
        | EndpointRequest::KeepSet { v }
        | EndpointRequest::Switch { v, .. }
        | EndpointRequest::Cancel { v }
        | EndpointRequest::Stop { v } => Some(*v),
        EndpointRequest::Unknown => None,
    };
    if version.is_some_and(|v| v != VERSION) {
        let _ = channel.send(&error("unsupported protocol version")).await;
        return;
    }
    match frame.message {
        EndpointRequest::Status { .. } => {
            let snapshot =
                serde_json::to_value(serve.shared.snapshot()).unwrap_or(serde_json::Value::Null);
            let _ = channel
                .send(&EndpointReply::Status {
                    v: VERSION,
                    snapshot,
                })
                .await;
        }
        EndpointRequest::KeepSet { .. } => {
            // From the snapshot, without the core. Without a set (not yet
            // computed, or not computable) the answer is an error: a caller
            // must not read an empty list as "nothing to keep".
            let reply = match serve.shared.snapshot().keep {
                Some(versions) => EndpointReply::KeepSet {
                    v: VERSION,
                    versions,
                },
                None => error("the keep set is not available"),
            };
            let _ = channel.send(&reply).await;
        }
        EndpointRequest::Cancel { .. } => {
            let reply = match serve.shared.request_cancel() {
                CancelAnswer::Cancelled => EndpointReply::Cancelled { v: VERSION },
                CancelAnswer::TooLate => EndpointReply::TooLate { v: VERSION },
                CancelAnswer::Nothing => refusal(
                    Some(EndpointErrorCode::NothingToCancel),
                    "nothing is switching",
                ),
            };
            let _ = channel.send(&reply).await;
        }
        EndpointRequest::Stop { .. } => {
            let _ = serve.commands.send(Command::Stop);
            let _ = channel.send(&EndpointReply::Stopping { v: VERSION }).await;
        }
        EndpointRequest::Switch {
            to,
            sha256,
            pin,
            how,
            auto,
            admitted,
            ..
        } => {
            if let Some(why) = serve.shared.snapshot().switching_disabled {
                // Answered from the snapshot, without the core (v3 3.8).
                tracing::debug!("switching is disabled: {why}");
                let _ = channel
                    .send(&refusal(
                        Some(EndpointErrorCode::SwitchingDisabled),
                        "switching is disabled; see the supervisor log",
                    ))
                    .await;
                return;
            }
            let (replies, mut inbox) = unbounded_channel();
            let request = SwitchRequest {
                to,
                sha256,
                pin,
                how: match how {
                    crate::protocol::How::Update => open_ferry_update::journal::How::Update,
                    crate::protocol::How::Rollback => open_ferry_update::journal::How::Rollback,
                },
                auto,
                admitted,
            };
            if serve
                .commands
                .send(Command::Switch { request, replies })
                .is_err()
            {
                let _ = channel.send(&error("the supervisor is stopping")).await;
                return;
            }
            // The switch goes on if the client goes away: only the stream ends.
            let mut answered = false;
            while let Some(reply) = inbox.recv().await {
                let last = matches!(
                    reply,
                    EndpointReply::Done { .. } | EndpointReply::Error { .. }
                );
                if channel.send(&reply).await.is_err() {
                    return;
                }
                if last {
                    answered = true;
                    break;
                }
            }
            if !answered {
                // The core dropped the sender without a last reply: it stopped
                // (A-N3) or panicked and started over (A-L8). Say so instead of
                // closing silently.
                let _ = channel
                    .send(&error(
                        "the supervisor stopped or restarted before this switch finished: check the status and try again",
                    ))
                    .await;
            }
        }
        EndpointRequest::Unknown => {
            let message_type = frame.message_type;
            let _ = channel
                .send(&EndpointReply::Unsupported {
                    v: VERSION,
                    message_type,
                })
                .await;
        }
    }
}

/// Accepts connections for the life of the supervisor, one task each. A failed
/// accept is logged and retried after a short wait; it never ends the loop.
pub async fn accept_loop(mut server: Server, serve: Serve) {
    loop {
        match server.accept().await {
            Ok(connection) => {
                let serve = serve.clone();
                tokio::spawn(async move {
                    handle(connection.stream, &serve).await;
                });
            }
            Err(error) => {
                tracing::warn!("the endpoint could not accept a connection: {error}");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Command;
    use crate::protocol::Admission;
    use open_ferry_update::journal::How;
    use tokio::io::duplex;

    fn serve() -> (Serve, tokio::sync::mpsc::UnboundedReceiver<Command>) {
        let (tx, rx) = unbounded_channel();
        (Serve::new(tx, Shared::new()), rx)
    }

    /// Sends `request`, runs the handler, returns every reply.
    async fn ask(serve: &Serve, request: serde_json::Value) -> Vec<EndpointReply> {
        let (client, server) = duplex(64 * 1024);
        let handler = {
            let serve = serve.clone();
            tokio::spawn(async move { handle(server, &serve).await })
        };
        let (reader, writer) = tokio::io::split(client);
        let mut channel = Channel::new(reader, writer);
        channel.send(&request).await.unwrap();
        let mut replies = Vec::new();
        while let Ok(Some(frame)) = channel.receive::<EndpointReply>().await {
            replies.push(frame.message);
        }
        handler.await.unwrap();
        replies
    }

    #[tokio::test]
    async fn status_answers_from_the_snapshot_without_the_core() {
        let (serve, _rx) = serve();
        serve.shared.publish(crate::snapshot::Snapshot {
            state: "watching".to_owned(),
            keep: Some(vec!["1.0.0".to_owned()]),
            ..crate::snapshot::Snapshot::default()
        });
        let replies = ask(&serve, serde_json::json!({"type": "status", "v": 1})).await;
        match &replies[..] {
            [EndpointReply::Status { snapshot, .. }] => {
                assert_eq!(snapshot["state"], "watching");
            }
            other => panic!("{other:?}"),
        }
        let replies = ask(&serve, serde_json::json!({"type": "keep_set", "v": 1})).await;
        assert_eq!(
            replies,
            [EndpointReply::KeepSet {
                v: 1,
                versions: vec!["1.0.0".to_owned()]
            }]
        );
    }

    #[tokio::test]
    async fn cancel_is_answered_from_the_snapshot() {
        let (serve, _rx) = serve();
        let replies = ask(&serve, serde_json::json!({"type": "cancel", "v": 1})).await;
        assert!(matches!(
            &replies[..],
            [EndpointReply::Error {
                code: Some(EndpointErrorCode::NothingToCancel),
                ..
            }]
        ));
        let mut snapshot = crate::snapshot::Snapshot::default();
        snapshot.switch = Some(crate::snapshot::SwitchView {
            id: "sw".to_owned(),
            how: "update".to_owned(),
            from: "1.0.0".to_owned(),
            to: "2.0.0".to_owned(),
            phase: "loading".to_owned(),
            status: "loading".to_owned(),
            since: "2026-10-09T12:00:00Z".to_owned(),
            cancellable: true,
            watch_until: None,
        });
        serve.shared.publish(snapshot.clone());
        let replies = ask(&serve, serde_json::json!({"type": "cancel", "v": 1})).await;
        assert_eq!(replies, [EndpointReply::Cancelled { v: 1 }]);
        assert!(serve.shared.cancel_requested());
        snapshot.switch.as_mut().unwrap().cancellable = false;
        serve.shared.publish(snapshot);
        let replies = ask(&serve, serde_json::json!({"type": "cancel", "v": 1})).await;
        assert_eq!(replies, [EndpointReply::TooLate { v: 1 }]);
    }

    #[tokio::test]
    async fn stop_reaches_the_core_and_unknown_and_old_requests_are_answered() {
        let (serve, mut rx) = serve();
        let replies = ask(&serve, serde_json::json!({"type": "stop", "v": 1})).await;
        assert_eq!(replies, [EndpointReply::Stopping { v: 1 }]);
        assert!(matches!(rx.recv().await, Some(Command::Stop)));
        let replies = ask(&serve, serde_json::json!({"type": "dance", "v": 1})).await;
        assert!(
            matches!(&replies[..], [EndpointReply::Unsupported { message_type, .. }] if message_type == "dance")
        );
        let replies = ask(&serve, serde_json::json!({"type": "status", "v": 9})).await;
        assert!(
            matches!(&replies[..], [EndpointReply::Error { reason, .. }] if reason.contains("version"))
        );
    }

    #[tokio::test]
    async fn a_bad_request_does_not_echo_private_paths() {
        let (serve, _rx) = serve();
        let replies = ask(
            &serve,
            serde_json::json!({
                "type": "switch", "v": 1, "to": "2.0.0", "sha256": "ab", "pin": "p",
                "how": "/private/update-test/invalid", "auto": false,
                "admitted": {"while_off": false, "mode_gen": 0},
            }),
        )
        .await;
        assert!(
            matches!(&replies[..], [EndpointReply::Error { reason, .. }] if reason == "bad request"),
            "{replies:?}"
        );
    }

    #[tokio::test]
    async fn a_switch_request_is_handed_to_the_core_and_its_replies_streamed() {
        let (serve, mut rx) = serve();
        let core = tokio::spawn(async move {
            let Some(Command::Switch { request, replies }) = rx.recv().await else {
                panic!("no switch command");
            };
            assert_eq!(request.to, "2.0.0");
            assert_eq!(request.how, How::Update);
            let _ = replies.send(EndpointReply::Accepted {
                v: 1,
                id: "sw-1".to_owned(),
            });
            let _ = replies.send(EndpointReply::Progress {
                v: 1,
                id: "sw-1".to_owned(),
                phase: "loading".to_owned(),
            });
            let _ = replies.send(EndpointReply::Done {
                v: 1,
                id: "sw-1".to_owned(),
                result: serde_json::json!({"result": "switched"}),
            });
        });
        let request = serde_json::json!({
            "type": "switch", "v": 1, "to": "2.0.0", "sha256": "ab", "pin": "p",
            "how": "update", "auto": false,
            "admitted": serde_json::to_value(Admission { while_off: false, mode_gen: 0 }).unwrap(),
        });
        let replies = ask(&serve, request).await;
        core.await.unwrap();
        assert!(matches!(
            &replies[..],
            [
                EndpointReply::Accepted { .. },
                EndpointReply::Progress { .. },
                EndpointReply::Done { .. }
            ]
        ));
    }

    #[tokio::test]
    async fn a_switch_while_switching_is_disabled_has_a_path_free_reason() {
        let (serve, mut rx) = serve();
        serve.shared.publish(crate::snapshot::Snapshot {
            switching_disabled: Some("the journal /private/account/state is unreadable".to_owned()),
            ..crate::snapshot::Snapshot::default()
        });
        let request = serde_json::json!({
            "type": "switch", "v": 1, "to": "2.0.0", "sha256": "ab", "pin": "p",
            "how": "update", "auto": false,
            "admitted": {"while_off": false, "mode_gen": 0},
        });
        let replies = ask(&serve, request).await;
        assert!(
            matches!(&replies[..], [EndpointReply::Error { code: Some(EndpointErrorCode::SwitchingDisabled), reason, .. }]
                if reason == "switching is disabled; see the supervisor log"),
            "{replies:?}"
        );
        assert!(rx.try_recv().is_err(), "the core was never asked");
    }

    // Exercise the actual core validator through the endpoint, with no worker spawn.
    async fn rollback_refusal(
        previous: Option<open_ferry_update::journal::Artifact>,
        to: &str,
        sha256: &str,
    ) {
        let mut rig = crate::testkit::Rig::new().await;
        rig.rt.as_mut().unwrap().journal.as_mut().unwrap().previous = previous.clone();
        let serve = Serve::new(rig.command_tx.clone(), rig.shared.clone());
        let request = serde_json::json!({
            "type": "switch", "v": 1, "to": to, "sha256": sha256, "pin": "",
            "how": "rollback", "auto": false,
            "admitted": {"while_off": false, "mode_gen": 0},
        });
        let mut core = crate::testkit::core_of!(rig);
        let run_core = async {
            let Some(Command::Switch { request, replies }) = core.commands.recv().await else {
                panic!("no switch command");
            };
            core.handle_switch(request, replies).await;
        };
        let (replies, ()) = tokio::join!(ask(&serve, request), run_core);
        let (code, reason) = if previous.is_none() {
            (
                EndpointErrorCode::NoPrevious,
                "there is no previous version to roll back to",
            )
        } else {
            (
                EndpointErrorCode::RollbackTargetMismatch,
                "that is not the version to roll back to",
            )
        };
        assert_eq!(
            replies,
            [EndpointReply::Error {
                v: VERSION,
                code: Some(code),
                reason: reason.into()
            }]
        );
        assert!(!core.rt.switching);
        assert!(rig.disk.order().is_empty());
        assert!(!rig.driver.log().iter().any(|line| line.contains("standby")));
    }

    #[tokio::test]
    async fn a_rollback_without_a_previous_version_has_a_typed_refusal() {
        rollback_refusal(None, "0.9.0", crate::testkit::SHA_3).await;
    }

    #[tokio::test]
    async fn a_rollback_with_a_different_version_or_hash_has_a_typed_refusal() {
        rollback_refusal(
            Some(crate::testkit::artifact("0.9.0")),
            "2.0.0",
            crate::testkit::SHA_2,
        )
        .await;
        rollback_refusal(
            Some(crate::testkit::artifact("0.9.0")),
            "0.9.0",
            crate::testkit::SHA_2,
        )
        .await;
    }

    #[tokio::test]
    async fn the_core_also_codes_a_switch_disabled_after_the_snapshot_was_read() {
        let mut rig = crate::testkit::Rig::new().await;
        rig.rt.as_mut().unwrap().disabled = Some("private journal path".into());
        let mut core = crate::testkit::core_of!(rig);
        let (tx, mut rx) = unbounded_channel();
        core.handle_switch(crate::testkit::update(false), tx).await;
        assert_eq!(
            crate::testkit::drain(&mut rx),
            [EndpointReply::Error {
                v: VERSION,
                code: Some(EndpointErrorCode::SwitchingDisabled),
                reason: "switching is disabled; see the supervisor log".into(),
            }]
        );
    }

    #[tokio::test]
    async fn a_waiting_switch_client_is_answered_when_the_core_drops_its_sender() {
        let (serve, mut rx) = serve();
        let core = tokio::spawn(async move {
            let Some(Command::Switch { replies, .. }) = rx.recv().await else {
                panic!("no switch command");
            };
            let _ = replies.send(EndpointReply::Accepted {
                v: 1,
                id: "sw-1".to_owned(),
            });
            // A panic drops the sender with the core.
            drop(replies);
        });
        let request = serde_json::json!({
            "type": "switch", "v": 1, "to": "2.0.0", "sha256": "ab", "pin": "p",
            "how": "update", "auto": false,
            "admitted": {"while_off": false, "mode_gen": 0},
        });
        let replies = ask(&serve, request).await;
        core.await.unwrap();
        assert!(
            matches!(&replies[..], [EndpointReply::Accepted { .. }, EndpointReply::Error { reason, .. }]
                if reason.contains("restarted")),
            "{replies:?}"
        );
    }

    #[tokio::test]
    async fn a_switch_request_with_the_core_gone_is_refused() {
        let (serve, rx) = serve();
        drop(rx);
        let request = serde_json::json!({
            "type": "switch", "v": 1, "to": "2.0.0", "sha256": "ab", "pin": "p",
            "how": "update", "auto": false,
            "admitted": {"while_off": false, "mode_gen": 0},
        });
        let replies = ask(&serve, request).await;
        assert!(
            matches!(&replies[..], [EndpointReply::Error { reason, .. }] if reason.contains("stopping"))
        );
    }
}
