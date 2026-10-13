// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_lifecycle.go and
// internal/runtime/executor/codex_websockets_executor_store_test.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use crate::auth::Auth;
use crate::exec::{AuthCheck, ExecError, Options, Request, Response, StreamResponse};
use crate::executor::ProviderExecutor;
use crate::manager::{Manager, Settings};
use futures_core::future::BoxFuture;
use std::sync::{Arc, Mutex};

struct Shared {
    closed: Arc<Mutex<Vec<String>>>,
    store: Arc<()>,
    removed: Arc<Mutex<Vec<String>>>,
}

impl ProviderExecutor for Shared {
    fn id(&self) -> &str {
        "codex"
    }
    fn execute(
        &self,
        _: Arc<Auth>,
        _: Request,
        _: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(async { unreachable!("lifecycle test does not execute requests") })
    }
    fn execute_stream(
        &self,
        _: Arc<Auth>,
        _: Request,
        _: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        Box::pin(async { unreachable!("lifecycle test does not execute requests") })
    }
    fn count_tokens(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        self.execute(auth, request, options)
    }
    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        Box::pin(async move { Ok((*auth).clone()) })
    }
    fn close_execution_session(&self, id: &str) {
        self.closed.lock().unwrap().push(id.to_owned());
    }
    fn execution_session_store(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        Some(self.store.clone())
    }
    fn close_auth_execution_sessions(&self, id: &str) {
        self.removed.lock().unwrap().push(id.to_owned());
    }
    fn interrupt_execution_session<'a>(
        &'a self,
        _: &'a str,
        _: &'a [u8],
        check: AuthCheck,
    ) -> BoxFuture<'a, Result<bool, ExecError>> {
        Box::pin(async move { Ok(check("credential")) })
    }
}

#[tokio::test]
async fn replacement_with_a_shared_store_keeps_sessions_and_removal_is_targeted() {
    let manager = Manager::new(
        Settings::default(),
        Arc::new(crate::registry::ModelRegistry::new()),
        None,
    );
    let previous = Arc::new(Mutex::new(Vec::new()));
    let current = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(());
    let removed = Arc::new(Mutex::new(Vec::new()));
    for closed in [&previous, &current] {
        manager.register_executor(Arc::new(Shared {
            closed: closed.clone(),
            store: store.clone(),
            removed: removed.clone(),
        }));
    }
    assert!(previous.lock().unwrap().is_empty());
    manager
        .register(Auth {
            id: "credential".into(),
            provider: "codex".into(),
            ..Auth::default()
        })
        .unwrap();
    assert!(
        manager
            .interrupt_execution_session("session", b"{}")
            .await
            .unwrap()
    );
    // The server reaches the manager through the trait, so the trait must
    // route there too rather than fall back to its default.
    let dispatcher: &dyn crate::exec::Dispatcher = &manager;
    assert!(
        dispatcher
            .interrupt_execution_session("session", b"{}")
            .await
            .unwrap()
    );
    let mut disabled = (*manager.get("credential").unwrap()).clone();
    disabled.status = crate::auth::Status::Disabled;
    assert!(!disabled.disabled);
    manager.update(disabled).unwrap();
    assert!(!manager.get("credential").unwrap().disabled);
    assert!(
        !dispatcher
            .interrupt_execution_session("session", b"{}")
            .await
            .unwrap()
    );
    let mut enabled = (*manager.get("credential").unwrap()).clone();
    enabled.status = crate::auth::Status::Active;
    manager.update(enabled).unwrap();
    assert!(
        dispatcher
            .interrupt_execution_session("session", b"{}")
            .await
            .unwrap()
    );
    manager.remove("credential");
    assert_eq!(*removed.lock().unwrap(), ["credential"]);
    assert!(current.lock().unwrap().is_empty());
    assert!(
        !manager
            .interrupt_execution_session("session", b"{}")
            .await
            .unwrap()
    );
    assert!(
        !dispatcher
            .interrupt_execution_session("session", b"{}")
            .await
            .unwrap()
    );
}
