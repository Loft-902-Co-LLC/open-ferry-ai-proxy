//! Native accepted-attempt contracts, tested with fake executors only.
use super::support::*;
use crate::exec::{Dispatcher, ExecError};
use crate::manager::Settings;

#[tokio::test(start_paused = true)]
async fn p47_truncation_is_one_attempt_without_cooldown() {
    for stream in [false, true] {
        let h = Harness::new(Settings {
            request_retry: 2,
            ..Default::default()
        });
        let executor = FakeExecutor::with("gemini", move |call| {
            let error = ExecError::stream_truncated("missing terminal frame");
            if call.kind == Kind::Stream {
                Reply::chunks(vec![Err(error)])
            } else {
                Reply::Err(error)
            }
        });
        h.executor(&executor);
        h.add(auth("a", "gemini"), &["m"]);
        h.add(auth("b", "gemini"), &["m"]);
        let error = if stream {
            match h
                .manager
                .execute_stream(&providers(&["gemini"]), request("m"), options())
                .await
            {
                Err(error) => error,
                Ok(stream) => collect(stream).await.1.expect("terminal failure"),
            }
        } else {
            h.manager
                .execute(&providers(&["gemini"]), request("m"), options())
                .await
                .unwrap_err()
        };
        assert_eq!(error.http_status(), 502);
        assert_eq!(error.failure_code(), Some("stream_truncated"));
        assert_eq!(
            executor
                .ids(if stream { Kind::Stream } else { Kind::Execute })
                .len(),
            1,
            "accepted generation replayed"
        );
        for id in ["a", "b"] {
            let live = h.get(id);
            assert!(!live.unavailable);
            assert!(live.model_states.is_empty());
        }
    }
}
