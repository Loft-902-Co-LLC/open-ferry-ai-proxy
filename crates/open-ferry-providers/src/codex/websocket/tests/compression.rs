use super::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;

mod wire {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../open-ferry-server/src/handlers/responses_ws/tests/compression_wire.rs"
    ));
}

#[tokio::test]
async fn round2_upstream_no_deflate_offer_and_plain_codex_works() {
    for setting in [None, Some(false), Some(true)] {
        let cfg = wire::configured(setting);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let head = wire::head(&mut tcp).await;
            assert!(
                wire::header(&head, "sec-websocket-extensions").is_none(),
                "upstream must not offer deflate: {head}"
            );
            let accept =
                derive_accept_key(wire::header(&head, "sec-websocket-key").unwrap().as_bytes());
            tcp.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
            let (opcode, payload) = wire::frame(&mut tcp, true).await;
            assert_eq!(opcode, 1);
            assert_eq!(parse_request(&payload)["type"], "response.create");
            wire::text(&mut tcp, COMPLETED, false).await;
            wire::close(&mut tcp, false).await;
        });
        let executor = executor_with(cfg);
        let response = within(
            "plain Codex call",
            executor.execute_stream(
                Arc::new(auth(&format!("http://{address}"))),
                request("gpt-5-codex", HELLO),
                with_header(
                    ws_options("plain-codex"),
                    "sec-websocket-extensions",
                    "permessage-deflate; client_max_window_bits",
                ),
            ),
        )
        .await
        .unwrap();
        let (chunks, error) = within("plain Codex response", collect(response)).await;
        assert!(error.is_none(), "{error:?}");
        assert!(!chunks.is_empty());
        within("plain mock Codex", server).await.unwrap();
        executor.close_execution_session("plain-codex");
    }
}

fn parse_request(payload: &[u8]) -> Value {
    serde_json::from_slice(payload).unwrap()
}
