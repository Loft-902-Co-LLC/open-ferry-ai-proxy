// Ported from CLIProxyAPI internal/cmd/siwc_bind.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Management binding uses the same leaf-link refusal and save-time identity
//! check as the CLI flow. The ID token's signature and nonce aren't verified,
//! and the SIWC account isn't matched to the outer account.

use std::io;
use std::path::PathBuf;
use open_ferry_core::manager::CompletedTokenCommand;

use open_ferry_core::auth::codex_refresh::{generate_device_id, validate_device_id};
use open_ferry_core::auth::credential_lock::acquire_credential_lock;
use open_ferry_core::auth::file_store::BindIdentity;
use open_ferry_core::auth::{Auth, CodexCredentialPartition};
use open_ferry_core::config::AuthFile;
use open_ferry_providers::codex::siwc::SiwcAuth;
use open_ferry_providers::codex::token::SiwcTokenData;
use open_ferry_providers::oauth::{Pkce, generate_state};
use serde_json::Value;

use crate::auth_files::run_blocking;
use crate::credential_files::is_unsafe_name;
use crate::state::ManagementState;

pub(super) struct Bind {
    pub(super) client: SiwcAuth,
    pub(super) target: Target,
    // Session-owned, never logged, and not used as a request identity.
    pub(super) nonce: String,
}

pub(super) struct Target {
    path: PathBuf,
    real_path: PathBuf,
    directory: PathBuf,
    identity: BindIdentity,
    record: Auth,
    registered: Auth,
    command: Option<CompletedTokenCommand>,
    pub(super) device_id: String,
}

fn invalid() -> io::Error {
    io::Error::other("invalid SIWC bind target")
}

pub(super) async fn prepare(state: &ManagementState, name: &str) -> io::Result<Bind> {
    if is_unsafe_name(name) || !name.ends_with(".json") {
        return Err(invalid());
    }
    let files = state
        .credential_store()
        .map_err(|_| invalid())?
        .files
        .clone();
    let mut matching = state.manager().list().into_iter().filter(|auth| auth.id == name || auth.file_name == name);
    let registered = matching.next().filter(|auth| eligible(auth)).filter(|_| matching.next().is_none()).ok_or_else(invalid)?;
    let command = state.manager().capture_token_command(&registered.id, CodexCredentialPartition::Siwc).ok_or_else(invalid)?;
    let registered = Auth::clone(&registered);
    let name = name.to_owned();
    let target = run_blocking(move || {
        let directory = files.base_dir().canonicalize()?;
        // Use the store's spelling for persistence and its credential lock;
        // canonical paths are only for containment and substitution checks.
        let path = files.file_path(&name)?;
        let real_path = directory.join(&name);
        let (metadata, file_id) = files.read_bind_target(&path)?;
        let identity = BindIdentity::capture(&metadata, file_id);
        if path.canonicalize()? != real_path {
            return Err(invalid());
        }
        let device_id = metadata
            .get("siwc")
            .and_then(|v| v.get("device_id"))
            .and_then(Value::as_str)
            .and_then(validate_device_id)
            .unwrap_or_else(generate_device_id);
        if files.resolve_auth_path(&registered)?.canonicalize()? != real_path { return Err(invalid()); }
        let mut record = registered.clone();
        record.metadata = metadata;
        Ok::<_, io::Error>(Target {
            path,
            real_path,
            directory,
            identity,
            record,
            registered,
            command: Some(command),
            device_id,
        })
    })
    .await?;
    let config = state.config();
    let client = SiwcAuth::with_proxy_url(&config.proxy_url)
        .with_endpoints(state.oauth_sessions().siwc_endpoints());
    Ok(Bind {
        client,
        target,
        nonce: generate_state(),
    })
}

impl Bind {
    pub(super) fn auth_url(&self, state: &str, redirect: &str, pkce: &Pkce) -> io::Result<String> {
        self.client
            .generate_auth_url(state, &self.nonce, &self.target.device_id, redirect, pkce)
            .map_err(|_| invalid())
    }
}

/// The v8 listing advertises only file-backed Codex OAuth targets.
pub(crate) fn eligible(auth: &Auth) -> bool {
    use open_ferry_core::auth::{AuthKind, AuthSource};
    auth.provider.trim().eq_ignore_ascii_case("codex")
        && auth.auth_source_kind() == Some(AuthSource::File)
        && auth.auth_kind() == Some(AuthKind::OAuth)
        && !auth.attribute("path").unwrap_or_default().trim().is_empty()
        && ["access_token", "refresh_token"].iter().all(|key| auth.metadata_str(key).is_some_and(|v| !v.trim().is_empty()))
}

impl Target {
    pub(super) fn name(&self) -> &str { &self.record.file_name }
}

pub(super) async fn save(
    state: &ManagementState,
    mut target: Target,
    tokens: SiwcTokenData,
    session: String,
    registration: u64,
) -> io::Result<()> {
    let store = state.credential_store().map_err(|_| invalid())?;
    let tokens = tokens.to_json();
    loop {
        if state.oauth_sessions().is_shut_down() || !state.oauth_sessions().store().is_pending(&session, "codex") { return Err(invalid()); }
        #[cfg(not(test))]
        let guard = acquire_credential_lock(&target.path).await?;
        #[cfg(test)]
        let guard = {
            use std::future::Future as _;
            let waiting = state.oauth_sessions().overrides().bind_lock_wait.clone();
            let mut acquire = Box::pin(acquire_credential_lock(&target.path));
            let mut signalled = false;
            std::future::poll_fn(|cx| {
                let result = acquire.as_mut().poll(cx);
                if result.is_pending() && !signalled {
                    if let Some(waiting) = &waiting { waiting.notify_one(); }
                    signalled = true;
                }
                result
            }).await?
        };
        let Ok(management_guard) = state.credential_lock().try_lock() else {
            drop(guard);
            drop(state.credential_lock().lock().await);
            continue;
        };
        let files = store.files.clone();
        let checking = state.clone();
        let session = session.clone();
        let tokens = tokens.clone();
        let (committed, returned, guard) = run_blocking(move || {
            let committed = checking.manager().try_commit_siwc_bind(&target.registered, || {
                if files.base_dir().canonicalize()? != target.directory || target.path.canonicalize()? != target.real_path { return Err(invalid()); }
                let record = files.bind_siwc_locked(&target.record, tokens, &guard, &|disk, id| {
                    if files.base_dir().canonicalize()? != target.directory { return Err(invalid()); }
                    target.identity.check(disk, id)?;
                    if checking.oauth_sessions().is_shut_down() || !checking.oauth_sessions().store().admit_bind(&session, registration) {
                        return Err(io::Error::other("SIWC bind session ended"));
                    }
                    Ok(())
                })?;
                let data = serde_json::to_vec(&record.metadata).map_err(|_| invalid())?;
                Ok((record, data))
            })?;
            Ok::<_, io::Error>((committed, target, guard))
        }).await?;
        target = returned;
        let Some((record, data)) = committed else {
            drop(guard);
            drop(management_guard);
            tokio::task::yield_now().await;
            continue;
        };
        let file = AuthFile { path: target.path, data: data.into() };
        let synced = match target.command.take() {
            Some(command) => store.sync.completed_token_command(record, Some(file), command),
            None => return Err(invalid()),
        };
        #[cfg(test)]
        if let Some(pause) = state.oauth_sessions().overrides().bind_after_sync.clone() {
            pause.0.notify_one();
            pause.1.notified().await;
        }
        drop(guard);
        drop(management_guard);
        return synced.await.map_err(|_| io::Error::other("SIWC credential sync failed"));
    }
}
