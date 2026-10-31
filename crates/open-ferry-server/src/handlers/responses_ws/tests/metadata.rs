use super::*;

#[tokio::test]
async fn p47_ws_metadata_uses_event_and_type_and_official_client_headers() {
    for (headers, codex) in [
        (vec![], false),
        (vec![("user-agent", "ordinary-client/1")], false),
        (vec![("user-agent", "codex_cli_rs/1")], true),
        (vec![("originator", "Codex Desktop/1")], true),
    ] {
        let frames = [
            "event: response.metadata\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"private-by-name\"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"codex.response.metadata\",\"marker\":\"private-by-type\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"done\",\"output\":[]}}\n\n",
        ];
        let (url, _) = serve(test_catalog(), vec![Outcome::chunks(&frames)]).await;
        let mut ws = connect(&url, &headers).await;
        send(
            &mut ws,
            r#"{"type":"response.create","model":"test-model","input":[]}"#,
        )
        .await;
        if codex {
            assert_eq!(recv(&mut ws).await["delta"], "private-by-name");
            assert_eq!(recv(&mut ws).await["marker"], "private-by-type");
        }
        assert_eq!(
            recv(&mut ws).await["type"],
            "response.completed",
            "headers={headers:?}"
        );
        ws.close(None).await.unwrap();
    }
}
