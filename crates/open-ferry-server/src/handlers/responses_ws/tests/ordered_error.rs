// Ported from TestForwardResponsesWebsocketPrefersBufferedErrorOverClosedData
// in CLIProxyAPI sdk/api/handlers/openai/openai_responses_websocket_test.go
// (0fe4cc2c, v8.0.47, MIT).
// Known difference ordered-result-stream: data and terminal errors share one
// Rust stream, so EOF cannot race a separately buffered error. This passes on
// the integration base; no production change or base-failing claim is needed.

use super::*;

#[tokio::test]
async fn buffered_terminal_error_precedes_eof_for_standard_and_duplex() {
    for duplex in [false, true] {
        for prefix in [None, Some("response.created"), Some("response.completed")] {
            for _ in 0..20 {
                let socket = FakeSocket::default();
                let mut conn = Conn::new(socket.clone());
                let (tx, rx) = tokio::sync::mpsc::channel(2);
                if let Some(kind) = prefix {
                    tx.send(Ok(Bytes::from(format!(
                        r#"{{"type":"{kind}","response":{{"id":"r1","output":[]}}}}"#
                    ))))
                    .await
                    .unwrap();
                }
                tx.send(Err(ErrorMessage::new(401, "credential failed")))
                    .await
                    .unwrap();
                drop(tx);
                let items = stream::unfold(rx, |mut rx| async {
                    rx.recv().await.map(|item| (item, rx))
                })
                .boxed();
                let caches = ServerToolCaches::default();
                let result = forward(
                    &mut conn,
                    items,
                    ForwardOptions {
                        caches: &caches,
                        principal: Principal::ANONYMOUS,
                        session_key: "buffered-error",
                        preserve_completion_output: false,
                        turn: None,
                        suppress_error: &|_| true,
                        keepalive: None,
                        context: None,
                        duplex,
                        websocket_owned: duplex,
                        is_codex_client: false,
                    },
                )
                .await;
                let Forwarded::Suppressed(error) = result else {
                    panic!("duplex={duplex} prefix={prefix:?}: {result:?}");
                };
                assert_eq!(error.status, 401);
                assert_eq!(error.text, "credential failed");
                assert_eq!(texts(&socket.sent()).len(), usize::from(prefix.is_some()));
                assert!(conn.write(br#"{"type":"still-open"}"#).await.is_ok());
            }
        }
    }
}
