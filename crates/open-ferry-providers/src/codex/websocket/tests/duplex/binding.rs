// Ported from TestResponsesSteeringInterceptorRewrittenBodyBindsClientModel
// in CLIProxyAPI sdk/api/handlers/openai/openai_responses_steering_auth_test.go
// (v8.0.47, MIT). No plugin hooks: exercise the rewritten input directly.

use super::*;

#[tokio::test]
async fn requested_client_model_survives_stripped_or_rewritten_payload() {
    for initial_model in [None, Some("rewritten-model")] {
        let server = serve(|mut peer| async move {
            if peer.recv().await.is_none() {
                return;
            }
            peer.send(&response("response.created", "r1")).await;
            peer.send(&response("response.completed", "r1")).await;
            if peer.recv().await.is_none() {
                return;
            }
            peer.send(&response("response.created", "r2")).await;
            peer.send(&response("response.completed", "r2")).await;
            if peer.recv().await.is_none() {
                return;
            }
            peer.send(&response("response.created", "r3")).await;
            peer.send(&response("response.completed", "r3")).await;
            peer.hold().await;
        })
        .await;
        let (mut client, input) = Client::new(8);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let input = input.with_turn_authorizer(Arc::new(move |payload| {
            let event: Value = serde_json::from_slice(payload).unwrap();
            assert_eq!(event["model"], "client-alias");
            let call = observed.fetch_add(1, Ordering::SeqCst);
            if call < 2 {
                Ok(Some(Bytes::from_static(
                    br#"{"type":"error","status":429,"error":{"message":"client alias denied"}}"#,
                )))
            } else {
                Ok(None)
            }
        }));
        let mut options = duplex_options("requested-model-binding", input);
        options.metadata.requested_model = " client-alias ".into();
        let mut payload = serde_json::json!({"input": []});
        if let Some(model) = initial_model {
            payload["model"] = Value::from(model);
        }
        let chunks = start(&steering(), &server.url, "A", &payload.to_string(), options).await;
        let mut denied = 0;
        let (_, error) = run(chunks, |event, _| match kind(event) {
            (kind, id) if kind == "response.completed" && id == "r1" => {
                client.send(r#"{"type":"response.create","input":[]}"#);
            }
            (kind, _) if kind == "error" => {
                assert_eq!(event["status"], 429);
                denied += 1;
                if denied == 1 {
                    client.send(r#"{"type":"response.steer","model":"spoof","previous_response_id":"r1","input":"denied"}"#);
                } else {
                    client.send(r#"{"type":"response.create","input":[]}"#);
                }
            }
            (kind, id) if kind == "response.completed" && id == "r2" => {
                client.send(r#"{"type":"response.create","model":"client-alias","input":[]}"#);
            }
            (kind, id) if kind == "response.completed" && id == "r3" => client.close(),
            _ => {}
        })
        .await;
        assert!(error.is_none(), "{initial_model:?}: {error:?}");
        assert_eq!(denied, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(messages(&server).len(), 3, "a rejected frame reached Codex");
        server.wait_closed(1).await;
    }
}
