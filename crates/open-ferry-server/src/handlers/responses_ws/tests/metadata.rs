use super::*;

#[tokio::test]
async fn round2_ws_metadata_matches_upstream_ws_rule() {
    for (headers, codex) in [
        (vec![], false),
        (vec![("user-agent", "ordinary-client/1")], false),
        (vec![("user-agent", "codex_cli_rs/1")], true),
        (vec![("originator", "Codex Desktop/1")], true),
    ] {
        let frames = [
            "event: response.metadata\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"by-name\"}\n\n",
            "data: {\"type\":\"codex.response.metadata\"}\n\n",
            "data: {\"type\":\"codex.rate_limits\"}\n\n",
            "data: {\"type\":\"responsesapi.private\"}\n\n",
            "data: {\"type\":\"response.subscription_usage\"}\n\n",
            "data: {\"type\":\"response.metadata\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"done\",\"output\":[]}}\n\n",
        ];
        let (url, _) = serve(test_catalog(), vec![Outcome::chunks(&frames)]).await;
        let mut ws = connect(&url, &headers).await;
        send(
            &mut ws,
            r#"{"type":"response.create","model":"test-model","input":[]}"#,
        )
        .await;
        assert_eq!(recv(&mut ws).await["delta"], "by-name");
        for kind in [
            "codex.response.metadata",
            "codex.rate_limits",
            "responsesapi.private",
        ] {
            assert_eq!(recv(&mut ws).await["type"], kind, "headers={headers:?}");
        }
        if codex {
            assert_eq!(recv(&mut ws).await["type"], "response.metadata");
        }
        assert_eq!(recv(&mut ws).await["type"], "response.completed");
        ws.close(None).await.unwrap();
    }
}
