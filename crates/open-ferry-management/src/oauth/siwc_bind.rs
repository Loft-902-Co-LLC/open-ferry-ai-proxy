// Ported from CLIProxyAPI internal/cmd/siwc_bind.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Management binding uses the same leaf-link refusal and save-time identity
//! check as the CLI flow. The ID token's signature and nonce aren't verified,
//! and the SIWC account isn't matched to the outer account.

use std::io;
use std::path::PathBuf;

use open_ferry_core::auth::codex_refresh::{generate_device_id, validate_device_id};
use open_ferry_core::auth::credential_lock::acquire_credential_lock;
use open_ferry_core::auth::file_store::{BindIdentity, read_siwc_bind_target};
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
    let name = name.to_owned();
    let target = run_blocking(move || {
        let directory = files.base_dir().canonicalize()?;
        // Use the store's spelling for persistence and its credential lock;
        // canonical paths are only for containment and substitution checks.
        let path = files.file_path(&name)?;
        let real_path = directory.join(&name);
        let (metadata, file_id) = read_siwc_bind_target(&path)?;
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
        let record = Auth {
            provider: "codex".into(),
            file_name: name,
            metadata,
            ..Auth::default()
        };
        Ok::<_, io::Error>(Target {
            path,
            real_path,
            directory,
            identity,
            record,
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

pub(super) async fn save(
    state: &ManagementState,
    target: Target,
    tokens: SiwcTokenData,
    session: String,
    registration: u64,
) -> io::Result<()> {
    let store = state.credential_store().map_err(|_| invalid())?;
    let files = store.files.clone();
    let (guard, management_guard) = loop {
        // A contended credential must not hold up unrelated management saves.
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
                    if let Some(waiting) = &waiting {
                        waiting.notify_one();
                    }
                    signalled = true;
                }
                result
            })
            .await?
        };
        if let Ok(management_guard) = state.credential_lock().try_lock() {
            break (guard, management_guard);
        }
        // Other management writers take their mutex first. Never wait for
        // it while holding the file lock, which would invert their order.
        drop(guard);
        drop(state.credential_lock().lock().await);
    };
    // Captured before the write, as a login's is: the bind completes a SIWC
    // token command for the credential registered at this file. The capture
    // takes only the manager's state lock, never a credential lock.
    let command = crate::token_record::capture_destination_command(
        state,
        &files,
        &target.record,
        CodexCredentialPartition::Siwc,
    );
    let checking = state.clone();
    let path = target.path.clone();
    let (record, data, guard) = run_blocking(move || {
        if files.base_dir().canonicalize()? != target.directory
            || target.path.canonicalize()? != target.real_path
        {
            return Err(invalid());
        }
        let record =
            files.bind_siwc_locked(&target.record, tokens.to_json(), &guard, &|disk, id| {
                if files.base_dir().canonicalize()? != target.directory {
                    return Err(invalid());
                }
                target.identity.check(disk, id)?;
                // Admission is the cancellation cutoff. The owned blocking
                // operation completes even if shutdown follows this point.
                if checking.oauth_sessions().is_shut_down()
                    || !checking
                        .oauth_sessions()
                        .store()
                        .admit_bind(&session, registration)
                {
                    return Err(io::Error::other("SIWC bind session ended"));
                }
                Ok(())
            })?;
        // Use the persisted record, never the pre-OAuth snapshot, for sync.
        let data = serde_json::to_vec(&record.metadata).map_err(|_| invalid())?;
        Ok::<_, io::Error>((record, data, guard))
    })
    .await?;
    let file = AuthFile {
        path,
        data: data.into(),
    };
    // A failed bind returned above, dropping its command unused.
    let synced = match command {
        Some(command) => store
            .sync
            .completed_token_command(record, Some(file), command),
        None => store.sync.file_written(file),
    };
    #[cfg(test)]
    {
        let pause = state.oauth_sessions().overrides().bind_after_sync.clone();
        if let Some(pause) = pause {
            pause.0.notify_one();
            pause.1.notified().await;
        }
    }
    drop(guard);
    drop(management_guard);
    synced
        .await
        .map_err(|_| io::Error::other("SIWC credential sync failed"))
}
