//! Versioned JSON-lines control and endpoint messages. EOF is not a message:
//! the worker must close listener copies first, finish admitted work and stop
//! within its budget. Only exit or a completed hand-over releases its lease.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{io, net::SocketAddr, path::PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Active,
    Standby,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installation {
    pub installed: PathBuf,
    pub data_dir: PathBuf,
    pub receipt: serde_json::Value,
}

/// Milliseconds, supplied by the binary that owns the actual timeouts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budgets {
    pub load_ms: u64,
    pub handover_ms: u64,
    pub ready_ms: u64,
    pub resume_ms: u64,
    pub watch_ms: u64,
    pub drain_ms: u64,
    pub stop_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerTopology {
    pub name: String,
    pub address: SocketAddr,
    pub tls: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsSocket {
    pub name: String,
    pub info: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello {
        v: u32,
        min_supervisor: u32,
        version: String,
        pid: u32,
        caps: Vec<String>,
    },
    Welcome {
        v: u32,
        role: Role,
        installation: Installation,
        budgets: Budgets,
        #[serde(default)]
        sockets: Vec<WindowsSocket>,
        topology: Vec<ListenerTopology>,
        switch_id: Option<String>,
    },
    Loaded {
        ms: u64,
    },
    LoadFailed {
        reason: String,
    },
    HandOver {
        budget_ms: u64,
    },
    HandedOver {
        state_id: String,
        live: u64,
    },
    Resume,
    Resumed,
    TakeOver,
    Ready,
    TakeFailed {
        reason: String,
    },
    Go,
    PeersGone,
    Release,
    Drained,
    Stop {
        budget_ms: u64,
    },
    Status {
        mode: String,
        live: u64,
        signins_pending: u64,
        phase: String,
    },
    Ping,
    Incompatible {
        needs_supervisor: u32,
    },
    Unsupported {
        message_type: String,
    },
    #[serde(other)]
    Unknown,
}

/// A newer worker offers its maximum and minimum in hello. It checks the
/// selected welcome version before acting; incompatibility never blames a release.
pub fn negotiate(supervisor_max: u32, worker_max: u32, worker_min: u32) -> Result<u32, Message> {
    let selected = supervisor_max.min(worker_max);
    if selected == 0 || worker_min == 0 || selected < worker_min {
        Err(Message::Incompatible {
            needs_supervisor: worker_min.max(1),
        })
    } else {
        Ok(selected)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    pub while_off: bool,
    pub mode_gen: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum How {
    Update,
    Rollback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EndpointRequest {
    Status {
        v: u32,
    },
    KeepSet {
        v: u32,
    },
    Switch {
        v: u32,
        to: String,
        sha256: String,
        pin: String,
        how: How,
        auto: bool,
        admitted: Admission,
    },
    Cancel {
        v: u32,
    },
    Stop {
        v: u32,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EndpointReply {
    Status {
        v: u32,
        snapshot: serde_json::Value,
    },
    KeepSet {
        v: u32,
        versions: Vec<String>,
    },
    Accepted {
        v: u32,
        id: String,
    },
    Progress {
        v: u32,
        id: String,
        phase: String,
    },
    Done {
        v: u32,
        id: String,
        result: serde_json::Value,
    },
    Cancelled {
        v: u32,
    },
    TooLate {
        v: u32,
    },
    Stopping {
        v: u32,
    },
    Error {
        v: u32,
        reason: String,
    },
    Unsupported {
        v: u32,
        message_type: String,
    },
    #[serde(other)]
    Unknown,
}

/// Carries the type separately so an unknown additive message can be answered
/// with unsupported rather than being mistaken for a transport failure.
#[derive(Debug)]
pub struct Frame<T> {
    pub message_type: String,
    pub message: T,
}

pub struct Channel<R, W> {
    reader: BufReader<R>,
    writer: W,
    pending: Vec<u8>,
    read_failed: bool,
    write_failed: bool,
}
impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Channel<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer,
            pending: Vec::new(),
            read_failed: false,
            write_failed: false,
        }
    }

    /// Cancellation-safe: a partial frame remains buffered for the next call.
    /// None is clean EOF. Partial-frame EOF is invalid, never an implicit command.
    /// A receive error makes further reads unusable; do not resynchronize.
    /// Writing a final Stop remains possible when the write half is healthy.
    pub async fn receive<T: DeserializeOwned>(&mut self) -> io::Result<Option<Frame<T>>> {
        if self.read_failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "control channel is unusable",
            ));
        }
        let result = self.receive_frame().await;
        if result.is_err() {
            self.read_failed = true;
        }
        result
    }

    async fn receive_frame<T: DeserializeOwned>(&mut self) -> io::Result<Option<Frame<T>>> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                return if self.pending.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "partial control frame",
                    ))
                };
            }
            let end = available.iter().position(|b| *b == b'\n');
            let used = end.map_or(available.len(), |i| i + 1);
            if self.pending.len().saturating_add(used) > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "control frame exceeds 64 KiB",
                ));
            }
            let part = available
                .get(..used)
                .ok_or_else(|| io::Error::other("invalid frame boundary"))?;
            self.pending.extend_from_slice(part);
            self.reader.consume(used);
            if end.is_some() {
                break;
            }
        }
        let value: serde_json::Value = serde_json::from_slice(&self.pending).map_err(invalid)?;
        self.pending.clear();
        let message_type = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "control frame has no type"))?
            .to_owned();
        let message = serde_json::from_value(value).map_err(invalid)?;
        Ok(Some(Frame {
            message_type,
            message,
        }))
    }

    /// Do not cancel a send: cancellation may leave a partial frame on the wire.
    /// A cancelled send or write error makes further writes unusable. Reads
    /// remain available so queued terminal messages are not lost after peer exit.
    pub async fn send<T: Serialize>(&mut self, message: &T) -> io::Result<()> {
        if self.write_failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "control channel is unusable",
            ));
        }
        let mut bytes = serde_json::to_vec(message).map_err(invalid)?;
        if bytes.len() >= MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control frame exceeds 64 KiB",
            ));
        }
        bytes.push(b'\n');
        self.write_failed = true;
        self.writer.write_all(&bytes).await?;
        self.writer.flush().await?;
        self.write_failed = false;
        Ok(())
    }
}
fn invalid(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Not upstream's: newer workers negotiate down, but required versions fail closed.
    #[test]
    fn versions() {
        assert_eq!(negotiate(1, 2, 1), Ok(1));
        assert_eq!(
            negotiate(1, 2, 2),
            Err(Message::Incompatible {
                needs_supervisor: 2
            })
        );
        assert!(negotiate(0, 1, 1).is_err());
    }
    // Not upstream's: additive fields and unknown types retain transport compatibility.
    #[tokio::test]
    async fn frames() {
        let bytes = b"{\"type\":\"ready\",\"future\":42}\n{\"type\":\"future\"}\n";
        let mut channel = Channel::new(&bytes[..], tokio::io::sink());
        assert_eq!(
            channel.receive::<Message>().await.unwrap().unwrap().message,
            Message::Ready
        );
        let unknown = channel.receive::<Message>().await.unwrap().unwrap();
        assert_eq!(unknown.message_type, "future");
        assert_eq!(unknown.message, Message::Unknown);
        assert!(channel.receive::<Message>().await.unwrap().is_none());
    }
    // Not upstream's: EOF midway, invalid UTF-8, missing type and size overflow are rejected.
    #[tokio::test]
    async fn malformed() {
        for bytes in [
            b"{".to_vec(),
            vec![255, b'\n'],
            b"{}\n".to_vec(),
            vec![b'x'; MAX_FRAME + 1],
        ] {
            let mut channel = Channel::new(&bytes[..], tokio::io::sink());
            assert!(channel.receive::<Message>().await.is_err());
        }
    }
    // Not upstream's: dropping a receive future preserves even a multi-buffer partial frame.
    #[tokio::test]
    async fn cancelled_receive_keeps_the_partial_frame() {
        let (mut sender, receiver) = tokio::io::duplex(MAX_FRAME);
        let mut partial = br#"{"type":"ready","padding":""#.to_vec();
        partial.extend(std::iter::repeat_n(b'x', 9000));
        sender.write_all(&partial).await.unwrap();
        let mut channel = Channel::new(receiver, tokio::io::sink());
        tokio::select! {
            biased;
            result = channel.receive::<Message>() => panic!("partial frame completed: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
        sender.write_all(b"\"}\n").await.unwrap();
        assert_eq!(
            channel.receive::<Message>().await.unwrap().unwrap().message,
            Message::Ready
        );
    }
    // Not upstream's: a failed or cancelled frame must not be followed by more wire traffic.
    #[tokio::test]
    async fn malformed_and_cancelled_send_poison_the_channel() {
        let mut channel = Channel::new(&b"{}\n{\"type\":\"ready\"}\n"[..], tokio::io::sink());
        assert!(channel.receive::<Message>().await.is_err());
        assert_eq!(
            channel.receive::<Message>().await.unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        let (writer, _reader) = tokio::io::duplex(1);
        let mut channel = Channel::new(tokio::io::empty(), writer);
        tokio::select! {
            biased;
            result = channel.send(&Message::Ready) => panic!("blocked send completed: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
        assert_eq!(
            channel.send(&Message::Ready).await.unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
    // Not upstream's: a failed write must not hide a terminal frame already queued by the peer.
    #[tokio::test]
    async fn failed_send_does_not_poison_receive() {
        let (writer, peer) = tokio::io::duplex(64);
        drop(peer);
        let mut channel = Channel::new(
            &b"{\"type\":\"incompatible\",\"needs_supervisor\":2}\n"[..],
            writer,
        );
        assert!(channel.send(&Message::Ping).await.is_err());
        assert_eq!(
            channel.receive::<Message>().await.unwrap().unwrap().message,
            Message::Incompatible {
                needs_supervisor: 2
            }
        );
        let mut channel = Channel::new(&b"{}\n"[..], tokio::io::sink());
        assert!(channel.receive::<Message>().await.is_err());
        channel
            .send(&Message::Stop { budget_ms: 100 })
            .await
            .unwrap();
    }
    // Not upstream's: all control messages and pin-bearing endpoint requests round trip.
    #[tokio::test]
    async fn round_trips() {
        let messages = vec![
            Message::Go,
            Message::PeersGone,
            Message::HandedOver {
                state_id: "id".into(),
                live: 2,
            },
            Message::Hello {
                v: 2,
                min_supervisor: 1,
                version: "0.3.0".into(),
                pid: 10,
                caps: vec![],
            },
            Message::Welcome {
                v: 1,
                role: Role::Standby,
                installation: Installation {
                    installed: "binary".into(),
                    data_dir: "data".into(),
                    receipt: serde_json::json!({}),
                },
                budgets: Budgets {
                    load_ms: 1,
                    handover_ms: 2,
                    ready_ms: 3,
                    resume_ms: 4,
                    watch_ms: 5,
                    drain_ms: 6,
                    stop_ms: 7,
                },
                sockets: vec![],
                topology: vec![ListenerTopology {
                    name: "main".into(),
                    address: "127.0.0.1:12345".parse().unwrap(),
                    tls: false,
                }],
                switch_id: None,
            },
            Message::Loaded { ms: 1 },
            Message::LoadFailed {
                reason: "error".into(),
            },
            Message::HandOver { budget_ms: 2 },
            Message::Resume,
            Message::Resumed,
            Message::TakeOver,
            Message::Ready,
            Message::TakeFailed {
                reason: "lease".into(),
            },
            Message::Release,
            Message::Drained,
            Message::Stop { budget_ms: 3 },
            Message::Status {
                mode: "auto".into(),
                live: 1,
                signins_pending: 0,
                phase: "active".into(),
            },
            Message::Ping,
            Message::Incompatible {
                needs_supervisor: 2,
            },
            Message::Unsupported {
                message_type: "future".into(),
            },
        ];
        for message in messages {
            let mut bytes = Vec::new();
            Channel::new(tokio::io::empty(), &mut bytes)
                .send(&message)
                .await
                .unwrap();
            let mut channel = Channel::new(&bytes[..], tokio::io::sink());
            assert_eq!(
                channel.receive::<Message>().await.unwrap().unwrap().message,
                message
            );
        }
        let request = EndpointRequest::Switch {
            v: 1,
            to: "0.3.0".into(),
            sha256: "hash".into(),
            pin: "pin".into(),
            how: How::Update,
            auto: true,
            admitted: Admission {
                while_off: false,
                mode_gen: 3,
            },
        };
        let json = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            serde_json::from_slice::<EndpointRequest>(&json).unwrap(),
            request
        );
    }
}
