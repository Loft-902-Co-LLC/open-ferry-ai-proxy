//! Final outbound payload rules reach both the wire and queued usage records.

use super::*;
use open_ferry_core::observe::usage::{Usage, reconfigure};
use open_ferry_core::observe::{Observation, Outcome, RequestContext};

#[tokio::test]
async fn finalizer_tier_reaches_wire_and_queue_for_execute_and_stream() {
    for stream in [false, true] {
        let mock = Mock::start(if stream {
            Reply::sse("data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"model\":\"custom-openai\",\"service_tier\":\"default\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n")
        } else {
            Reply::json(r#"{"id":"chatcmpl_1","object":"chat.completion","model":"custom-openai","service_tier":"default","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#)
        }).await;
        let config = Arc::new(Config::parse("proxy-url: direct\nusage-statistics-enabled: true\npayload:\n  override:\n    - models:\n        - name: '*'\n          protocol: openai\n      params:\n        service_tier: priority\n").unwrap());
        let usage = Usage::new(&config);
        reconfigure(&usage, None, &config, true);
        let request = request(
            "custom-openai",
            r#"{"model":"custom-openai","service_tier":"auto","messages":[{"role":"user","content":"hi"}]}"#,
        );
        let mut options = Options::new(Format::OPENAI);
        options.original_request = request.payload.clone();
        options.stream = stream;
        options.metadata.request_path = "/v1/chat/completions".into();
        let context = Arc::new(RequestContext::new(
            http::Method::POST,
            "/v1/chat/completions".into(),
        ));
        let tap = usage.tap(&context, &request, &options).unwrap();
        options.observation = Some(Arc::new(Observation::new(context, vec![tap.clone()])));
        let auth = Arc::new(compat_auth(&mock.base_url(), "openai-compatibility"));
        let executor = OpenAiCompatExecutor::new("openai-compatibility", config);
        if stream {
            let mut response = executor
                .execute_stream(auth, request, options)
                .await
                .unwrap();
            while let Some(chunk) = response.chunks.next().await {
                chunk.unwrap();
            }
        } else {
            executor.execute(auth, request, options).await.unwrap();
        }
        tap.finish(Outcome::Completed);
        assert_eq!(mock.last().json()["service_tier"], "priority");
        let records = usage.pop_oldest(10);
        assert_eq!(records.len(), 1);
        let record: Value = serde_json::from_slice(&records[0]).unwrap();
        assert_eq!(record["service_tier"], "auto");
        assert_eq!(record["effective_service_tier"], "priority");
        assert_eq!(record["response_service_tier"], "default");
        assert_eq!(record["reasoning_effort"], "");
        assert_eq!(record["tokens"]["total_tokens"], 2);
    }
}
