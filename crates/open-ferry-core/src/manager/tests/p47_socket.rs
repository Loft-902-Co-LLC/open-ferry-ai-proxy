//! Upstream loss terminates a turn without credential policy or replay.
use super::support::*;
use crate::exec::{Dispatcher, ExecError};
use crate::manager::Settings;

#[tokio::test(start_paused = true)]
async fn p47_socket_loss_never_replays_or_cools_a_credential() {
    for path in [0, 1, 2] {
        let h = Harness::new(Settings {
            request_retry: 2,
            ..Default::default()
        });
        let executor = FakeExecutor::with("codex", move |_| {
            let error = ExecError::upstream_websocket_lost();
            match path {
                1 => Reply::chunks(vec![Err(error)]),
                2 => Reply::chunks(vec![Ok("accepted".into()), Err(error)]),
                _ => Reply::Err(error),
            }
        });
        h.executor(&executor);
        for id in ["a", "b"] {
            let mut a = auth(id, "codex");
            a.metadata.insert("request_scoped_errors".into(),serde_json::json!([{"status":502,"match":["websocket"],"action":"continue-and-cooldown"}]));
            h.add(a, &["m"]);
        }
        let error = match h
            .manager
            .execute_stream(&providers(&["codex"]), request("m"), options())
            .await
        {
            Err(e) => e,
            Ok(stream) => collect(stream).await.1.unwrap(),
        };
        assert_eq!(error.http_status(), 502);
        assert!(error.stops_replay());
        assert!(error.ws_close.is_none());
        assert_eq!(
            executor.ids(Kind::Stream).len(),
            1,
            "lost generation replayed"
        );
        for id in ["a", "b"] {
            let a = h.get(id);
            assert!(!a.unavailable);
            assert!(a.model_states.is_empty());
        }
    }
}
