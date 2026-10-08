// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_videos_handlers.go
// (videoContentHTTPClient, videoContentDownloadAuth) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The manager's [`Dispatcher::download`]: a file a provider made, fetched
//! by that provider's executor through the proxy of the credential that
//! made it.
//!
//! The credential is looked up by its trimmed ID, whatever its state, as
//! upstream's `GetByID` finds it; with no ID, or one the manager doesn't
//! hold, the fetch goes through the global proxy. The download's provider
//! names the executor that fetches the file, whatever the credential's own
//! provider, which only lends its proxy. Nothing is recorded on the
//! credential, and no other credential is tried.
//!
//! Deviations from upstream: the handler's own HTTP client becomes the
//! executor's (see [`crate::exec::Download`]). An error with no status of
//! its own is a 502, as upstream's `HTTPStatusFromErrorOr` makes one.
//!
//! [`Dispatcher::download`]: crate::exec::Dispatcher::download

use crate::exec::{Download, Downloaded, ErrorKind, ExecError};

use super::Manager;

impl Manager {
    /// Fetches `download` (see the module docs).
    pub(super) async fn download(&self, download: Download) -> Result<Downloaded, ExecError> {
        let Download {
            provider,
            auth_id,
            url,
        } = download;
        let Some(executor) = self.executor(provider.trim()) else {
            let message = format!("no executor for provider {}", provider.trim());
            return Err(ExecError::new(ErrorKind::Upstream, message).with_status(502));
        };
        let auth_id = auth_id.trim();
        let auth = (!auth_id.is_empty()).then(|| self.get(auth_id)).flatten();
        executor.download(auth, url).await.map_err(|error| {
            if error.http_status() > 0 {
                error
            } else {
                error.with_status(502)
            }
        })
    }
}
