use super::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

#[path = "compression_wire.rs"]
mod wire;

struct ListenerTask(tokio::task::JoinHandle<()>);
impl Drop for ListenerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn round2_downstream_deflate_offers_get_plain_sessions() {
    for setting in [None, Some(false), Some(true)] {
        for offer in [
            "permessage-deflate; client_max_window_bits",
            "permessage-deflate; server_no_context_takeover; client_no_context_takeover",
            "permessage-deflate; client_max_window_bits=15; server_max_window_bits=15",
        ] {
            let cfg = wire::configured(setting);
            let mut config = ServerConfig::from(&cfg);
            config.api_keys = vec!["sk-test".into()];
            let dispatcher =
                FakeDispatcher::new(vec![completes("plain-1", "[]"), completes("plain-2", "[]")]);
            let state = crate::testing::state(config, test_catalog(), &dispatcher);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let _listener = ListenerTask(tokio::spawn(async move {
                axum::serve(listener, crate::router(state)).await.unwrap();
            }));
            let mut tcp = TcpStream::connect(address).await.unwrap();
            let request = format!(
                "GET /v1/responses HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nAuthorization: Bearer sk-test\r\nSec-WebSocket-Extensions: {offer}\r\n\r\n"
            );
            tcp.write_all(request.as_bytes()).await.unwrap();
            let head = wire::head(&mut tcp).await;
            assert!(head.starts_with("HTTP/1.1 101"), "{head}");
            assert!(
                wire::header(&head, "sec-websocket-extensions").is_none(),
                "setting={setting:?} offer={offer}: {head}"
            );
            for id in ["plain-1", "plain-2"] {
                wire::text(
                    &mut tcp,
                    r#"{"type":"response.create","model":"test-model","input":[]}"#,
                    true,
                )
                .await;
                let (opcode, payload) = wire::frame(&mut tcp, false).await;
                assert_eq!(opcode, 1);
                let event = parse(&payload);
                assert_eq!(event["type"], "response.completed");
                assert_eq!(event["response"]["id"], id);
            }
            wire::close(&mut tcp, true).await;
        }
    }
}
