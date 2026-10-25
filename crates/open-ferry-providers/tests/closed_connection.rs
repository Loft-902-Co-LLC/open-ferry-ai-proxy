//! Real executor requests against loopback-only peers. HTTP/2 frame and
//! source-chain coverage lives in the shared transport unit tests.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{ExecError, Format, Options, Request, TransportFault};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_providers::claude::ClaudeExecutor;
use open_ferry_providers::codex::CodexExecutor;
use open_ferry_providers::gemini::{GeminiExecutor, InteractionsExecutor, VertexExecutor};
use open_ferry_providers::meta::MetaExecutor;
use open_ferry_providers::openai_compat::OpenAiCompatExecutor;
use open_ferry_providers::xai::XaiExecutor;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Copy, Debug)]
enum Family {
    Claude,
    Codex,
    Gemini,
    Interactions,
    Vertex,
    VertexInteractions,
    Xai,
    Compat,
    Meta,
}

impl Family {
    fn executor(self) -> Arc<dyn ProviderExecutor> {
        match self {
            Self::Claude => Arc::new(ClaudeExecutor::new("direct")),
            Self::Codex => Arc::new(CodexExecutor::new("direct")),
            Self::Gemini => Arc::new(GeminiExecutor::new("direct")),
            Self::Interactions => Arc::new(InteractionsExecutor::new("direct")),
            Self::Vertex | Self::VertexInteractions => Arc::new(VertexExecutor::new("direct")),
            Self::Xai => Arc::new(XaiExecutor::new("direct")),
            Self::Compat => {
                let mut config = Config::default();
                config.proxy_url = "direct".into();
                Arc::new(OpenAiCompatExecutor::new("local-compat", Arc::new(config)))
            }
            Self::Meta => Arc::new(MetaExecutor::new("direct")),
        }
    }

    fn request(self, sending: bool) -> (Request, Options) {
        let text = if sending {
            "x".repeat(16 * 1024 * 1024)
        } else {
            "hello".into()
        };
        let (model, format, body) = match self {
            Self::Claude => (
                "claude-sonnet-4-5",
                Format::CLAUDE,
                json!({"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{"role":"user","content":text}]}),
            ),
            Self::Gemini | Self::Vertex => (
                "gemini-2.5-flash",
                Format::GEMINI,
                json!({"contents":[{"role":"user","parts":[{"text":text}]}]}),
            ),
            Self::Interactions | Self::VertexInteractions => (
                "gemini-2.5-flash",
                Format::INTERACTIONS,
                json!({"model":"gemini-2.5-flash","input":text}),
            ),
            Self::Compat => (
                "local-model",
                Format::OPENAI,
                json!({"model":"local-model","messages":[{"role":"user","content":text}]}),
            ),
            _ => (
                "gpt-5",
                Format::OPENAI_RESPONSE,
                json!({"model":"gpt-5","input":text}),
            ),
        };
        (
            Request {
                model: model.into(),
                payload: Bytes::from(body.to_string()),
            },
            Options::new(format),
        )
    }
}

#[derive(Clone, Copy, Debug)]
enum Case {
    BeforeResponse,
    MidBody,
    Sending,
    StreamBody,
}

/// Read a complete request before a graceful close, so it isn't turned into a
/// reset by dropping a socket with unread upload data.
async fn request_head(socket: &mut TcpStream, sending: bool) {
    let mut raw = Vec::new();
    while !raw.ends_with(b"\r\n\r\n") {
        raw.push(socket.read_u8().await.unwrap());
    }
    if !sending {
        let head = String::from_utf8(raw).unwrap();
        let length: usize = head
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap_or_default();
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
    }
}

async fn exercise(family: Family, case: Case) {
    tokio::time::timeout(Duration::from_secs(20), async {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let sending = matches!(case, Case::Sending);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            request_head(&mut socket, sending).await;
            if matches!(case, Case::MidBody | Case::StreamBody) {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 100\r\n\r\nx").await.unwrap();
            }
            if sending {
                socket2::SockRef::from(&socket).set_linger(Some(Duration::ZERO)).unwrap();
            } else {
                socket.shutdown().await.unwrap();
            }
        });
        let executor = family.executor();
        let mut auth = Auth { provider: executor.id().into(), proxy_url: "direct".into(), ..Auth::default() };
        auth.attributes.insert("base_url".into(), format!("http://{address}"));
        auth.attributes.insert("api_key".into(), "local-fixture-key".into());
        let (request, mut options) = family.request(sending);
        // Codex's ordinary Responses reader deliberately turns truncation into
        // a request-scoped 408. Compact returns the original body-read error.
        if matches!(family, Family::Codex) && matches!(case, Case::MidBody) {
            options.alt = "responses/compact".into();
        }
        let error = if matches!(case, Case::StreamBody) {
            options.stream = true;
            match executor.execute_stream(Arc::new(auth), request, options).await {
                Err(error) => error,
                Ok(mut response) => {
                    let mut failure = None;
                    while let Some(chunk) = response.chunks.next().await {
                        if let Err(error) = chunk { failure = Some(error); break; }
                    }
                    failure.unwrap_or_else(ExecError::empty_stream)
                }
            }
        } else {
            executor.execute(Arc::new(auth), request, options).await.unwrap_err()
        };
        server.await.unwrap();
        if matches!(family, Family::Codex) && matches!(case, Case::StreamBody) {
            // Existing provider-specific incomplete/empty-stream behavior is
            // intentionally not replaced by generic transport classification.
            assert!(error.status == 408 || error.kind == open_ferry_core::exec::ErrorKind::EmptyStream, "{error:?}");
        } else {
            let want = if sending { TransportFault::Transient } else { TransportFault::Lifecycle };
            assert_eq!(error.transport, Some(want), "{family:?} {case:?}: {error:?}");
        }
    }).await.expect("local executor fixture timed out");
}

macro_rules! family_tests {
    ($name:ident, $family:ident) => {
        mod $name {
            use super::*;
            #[tokio::test]
            async fn closed_before_response() {
                exercise(Family::$family, Case::BeforeResponse).await;
            }
            #[tokio::test]
            async fn closed_mid_body() {
                exercise(Family::$family, Case::MidBody).await;
            }
            #[tokio::test]
            async fn closed_while_sending() {
                exercise(Family::$family, Case::Sending).await;
            }
            #[tokio::test]
            async fn closed_stream_body() {
                exercise(Family::$family, Case::StreamBody).await;
            }
        }
    };
}
family_tests!(claude, Claude);
family_tests!(codex, Codex);
family_tests!(gemini, Gemini);
family_tests!(interactions, Interactions);
family_tests!(vertex, Vertex);
family_tests!(vertex_interactions, VertexInteractions);
family_tests!(xai, Xai);
family_tests!(compat, Compat);
family_tests!(meta, Meta);
