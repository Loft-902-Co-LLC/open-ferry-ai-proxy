// Round 1: bounded, real-router interleavings against local Codex mocks.
// Coverage currently includes early 429 versus accepted output, a terminal
// event, queued next input and downstream disconnect, with steering on/off.
// Transport-loss epochs and interrupt/terminal races are NOT covered yet.

use super::*;

const ROUND1_INVARIANT: &str = "no accepted output replayed, each loss reported at most once, no interrupt applied after a terminal event, and the downstream socket open exactly when upstream keeps it open";

async fn retry_proxy(config: &Config, first: &str, sibling: &str) -> String {
    let registry = Arc::new(ModelRegistry::new());
    let manager = Arc::new(Manager::new(Settings::default(), registry.clone(), None));
    manager.register_executor(Arc::new(
        CodexExecutor::new("direct").with_config(Arc::new(config.clone())),
    ));
    let models = [ModelInfo {
        id: "round1-invariant-model".into(),
        ..Default::default()
    }];
    for (id, url, priority) in [("first", first, "10"), ("sibling", sibling, "0")] {
        registry.register_client(id, "codex", &models);
        let mut auth = credential(id, url, false);
        auth.attributes.insert("priority".into(), priority.into());
        manager.register_unsaved(auth).unwrap();
    }
    let server = ServerConfig {
        api_keys: vec!["sk-test".into()],
        ..ServerConfig::from(config)
    };
    let catalog = FakeCatalog::new().serve("round1-invariant-model", &["codex"]);
    let app = crate::router(AppState::new(server, manager, Arc::new(catalog)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("ws://{addr}/v1/responses")
}

#[tokio::test]
async fn round1_invariant_family_early_429_output_terminal_queued_disconnect() {
    for duplex in [false, true] {
        for terminal_error in [false, true] {
            for accepted_output in [false, true] {
                for queued in [false, true] {
                    let context = format!(
                        "duplex={duplex}, failed={terminal_error}, accepted={accepted_output}, queued={queued}: {ROUND1_INVARIANT}"
                    );
                    let mut rejected = Upstream::start(move |mut peer| async move {
                        peer.read().await.unwrap();
                        if accepted_output {
                            peer.write(r#"{"type":"response.created","response":{"id":"accepted"}}"#).await;
                            peer.write(r#"{"type":"response.output_text.delta","delta":"accepted-once"}"#).await;
                        } else if !duplex {
                            // Non-duplex bootstrap holds this frame back; it is not accepted output.
                            peer.write(r#"{"type":"response.created","response":{"id":"buffered"}}"#).await;
                        }
                        let failure = if terminal_error {
                            r#"{"type":"response.failed","response":{"id":"accepted","status":"failed","error":{"type":"usage_limit_reached","message":"quota exhausted","resets_in_seconds":120}}}"#
                        } else {
                            r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"quota exhausted","resets_in_seconds":120}}"#
                        };
                        peer.write(failure).await;
                        peer.read().await;
                    }).await;
                    let mut sibling = Upstream::start(|mut peer| async move {
                        for id in ["winner-1", "winner-2"] {
                            if peer.read().await.is_none() { return; }
                            peer.write(&format!(r#"{{"type":"response.created","response":{{"id":"{id}"}}}}"#)).await;
                            peer.write(&format!(r#"{{"type":"response.completed","response":{{"id":"{id}","output":[]}}}}"#)).await;
                        }
                        peer.read().await;
                    }).await;
                    let mut config = steering(duplex);
                    config.codex.stream_bootstrap_buffering = true;
                    let url = retry_proxy(&config, &rejected.url, &sibling.url).await;
                    let mut ws = connect(&url, &[]).await;
                    let create =
                        r#"{"type":"response.create","model":"round1-invariant-model","input":[]}"#;
                    send(&mut ws, create).await;
                    if queued {
                        send(&mut ws, create).await;
                    }
                    if accepted_output {
                        let (events, _) = rest(&mut ws).await;
                        assert_eq!(
                            events
                                .iter()
                                .filter(|e| e["type"] == "response.output_text.delta")
                                .count(),
                            1,
                            "{context}"
                        );
                        assert_eq!(sibling.connections(), 0, "{context}");
                        assert!(
                            (1..=1 + usize::from(queued && duplex)).contains(&rejected.frames()),
                            "{context}"
                        );
                    } else {
                        for id in ["winner-1", "winner-2"] {
                            let created = recv(&mut ws).await;
                            assert_eq!(created["type"], "response.created", "{context}: {created}");
                            assert_eq!(created["response"]["id"], id, "{context}");
                            assert_eq!(
                                recv(&mut ws).await["type"],
                                "response.completed",
                                "{context}"
                            );
                            if id == "winner-1" && !queued {
                                send(&mut ws, create).await;
                            }
                        }
                        assert_eq!(rejected.frames(), 1, "{context}");
                        assert_eq!(sibling.frames(), 2, "{context}");
                        ws.close(None).await.unwrap();
                        sibling.finished().await;
                    }
                    rejected.finished().await;
                }
            }
        }
    }
}
