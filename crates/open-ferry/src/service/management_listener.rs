//! open-ferry's own: the management address's listener
//! (`management.separate-address`). Upstream has no such setting.
//!
//! With the setting, the service listens on two addresses:
//! - The proxy's (`server.host` and `server.port`) serves the proxy as
//!   before, with `/`, `/healthz`, the keep-alive endpoint and the main
//!   server's OAuth callback pages (`/anthropic/callback` and
//!   `/codex/callback`, where the callback forwarders send the browser).
//!   The management API, the dashboard and the dashboard API aren't served
//!   there: every path under `/v0/management` and `/v8/management` answers
//!   an empty 404, as while no management key is set; the dashboard app
//!   and `/management.html` an empty 404, as while
//!   `management.disable-control-panel` is set; and the dashboard API's
//!   routes `management_disabled`, as while no management key is set.
//! - The management address serves the management API, the dashboard and
//!   the dashboard API alone, with the same checks: the management key,
//!   `allow-remote` for a client that isn't local, and the bans, which the
//!   two share. Every other path, the proxy's included, answers gin's 404.
//!
//! Both pass through the same request context, logging, CORS and panic
//! handling, use `server.tls` when it is on, and stop together, giving
//! open requests the same time. The address is bound right after the
//! proxy's, and either failing to bind stops the start. Changing the
//! setting takes a restart, as changing `host`, `port` or `tls` does; a
//! reload logs that it was ignored.

use std::future::Future;
use std::io;
use std::sync::Arc;

use axum::Router;
use open_ferry_core::config::{Config, ManagementAddress};
use open_ferry_dashboard::Listener;
use open_ferry_server::{router_with, router_without_proxy};
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::{Server, Service, bind, serve};

/// The management address, bound.
pub(super) struct Separate {
    pub(super) address: ManagementAddress,
    listener: TcpListener,
}

/// Binds the management address of `config`, if it has one. The error is
/// the line to log.
pub(super) fn bind_separate(config: &Config) -> Result<Option<Separate>, String> {
    let address = match config.remote_management.separate_address() {
        Ok(Some(address)) => address,
        Ok(None) => return Ok(None),
        Err(error) => return Err(format!("management.separate-address: {error}")),
    };
    match bind(&address.host, i64::from(address.port)) {
        Ok(listener) => Ok(Some(Separate { address, listener })),
        Err(error) => Err(format!(
            "failed to start the management server on {address}: {error}"
        )),
    }
}

/// Serves `service` on `main`, the proxy's listener, and `separate`, each
/// with `tls` when it is on, until `stopped` turns true. `extra` is served
/// on the proxy's.
pub(super) fn spawn(
    service: &Service,
    main: TcpListener,
    separate: Separate,
    tls: Option<Arc<rustls::ServerConfig>>,
    extra: Router,
    stopped: watch::Receiver<bool>,
) -> Server {
    let (main_app, management_app) = apps(service, extra);
    tokio::spawn(serve_both(
        serve(main, tls.clone(), main_app, stopped.clone()),
        serve(separate.listener, tls, management_app, stopped),
    ))
}

/// The routes of the proxy's listener, with `extra`, and of the management
/// address's.
pub(super) fn apps(service: &Service, extra: Router) -> (Router, Router) {
    let management = &service.management;
    let ledger = &service.ledger;
    let closed = open_ferry_management::pages_router(management.clone()).merge(
        open_ferry_dashboard::router_for(management.clone(), ledger.clone(), Listener::Closed),
    );
    let main = router_with(service.state.clone(), closed.merge(extra));
    let served = open_ferry_management::api_router(management.clone()).merge(
        open_ferry_dashboard::router_for(management.clone(), ledger.clone(), Listener::Separate),
    );
    let separate = router_without_proxy(service.state.clone(), served);
    (main, separate)
}

/// Runs both servers until both have stopped, or one fails: then the other
/// is dropped, and the error is the failed one's, a management server's
/// said to be.
pub(super) async fn serve_both(
    main: impl Future<Output = io::Result<()>>,
    management: impl Future<Output = io::Result<()>>,
) -> io::Result<()> {
    let management = async {
        management
            .await
            .map_err(|error| io::Error::new(error.kind(), format!("management server: {error}")))
    };
    tokio::try_join!(main, management).map(|((), ())| ())
}

/// Whether a reload from `previous` to `config` changed the management
/// address, which takes a restart.
pub(super) fn changed(previous: &Config, config: &Config) -> bool {
    previous.remote_management.separate_address != config.remote_management.separate_address
}

/// Logs that a reload's change of the management address waits for a
/// restart.
pub(super) fn warn_on_change(previous: &Config, config: &Config) {
    if changed(previous, config) {
        tracing::warn!("management.separate-address changes take effect after a restart");
    }
}

#[cfg(test)]
mod tests;
