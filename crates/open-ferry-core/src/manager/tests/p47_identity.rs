//! Owned request identity persists through asynchronous credential retries.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use http::Method;
use super::support::*;
use crate::exec::Dispatcher;
use crate::observe::{Observation, RequestContext};
use crate::manager::Settings;

#[tokio::test(start_paused = true)]
async fn p47_identity_and_codex_flag_survive_async_retry_including_empty_identity() {
    for key in ["", "dummy-client"] {
        let h=Harness::new(Settings::default());
        let context=Arc::new(RequestContext::new(Method::POST,"/v1/responses".into()));
        context.set_client_key(key);
        context.set_access_provider("config-api-key");
        context.set_is_native_key(true);
        let observation=Arc::new(Observation::new(context.clone(),vec![]));
        let expected=observation.clone();
        let count=Arc::new(AtomicUsize::new(0));
        let seen=count.clone();
        let executor=FakeExecutor::with("codex",move |call| {
            let incoming=call.options.observation.as_ref().unwrap();
            assert!(Arc::ptr_eq(incoming,&expected));
            assert_eq!(incoming.context().client_key(),Some(key));
            incoming.context().set_client_key("other-dummy");
            assert_eq!(incoming.context().client_key(),Some(key));
            assert!(incoming.context().is_native_key());
            assert!(call.options.is_codex_client_request());
            if seen.fetch_add(1,Ordering::SeqCst)==0 {Reply::status(503,"temporary")} else {Reply::ok("ok")}
        });
        h.executor(&executor);
        h.add(auth("a","codex"),&["m"]);
        h.add(auth("b","codex"),&["m"]);
        let mut opts=options();
        opts.observation=Some(observation);
        opts.headers.insert("user-agent", "codex_cli_rs/1.0".parse().unwrap());
        let manager=h.manager.clone();
        tokio::spawn(async move {manager.execute(&providers(&["codex"]),request("m"),opts).await}).await.unwrap().unwrap();
        assert_eq!(count.load(Ordering::SeqCst),2);
        assert_eq!(context.client_key(),Some(key));
    }
}
