// Ported from CLIProxyAPI internal/cmd/siwc_bind.go and
// internal/api/handlers/management/oauth_siwc.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Management binding uses the same leaf-link refusal and save-time identity
//! check as the CLI flow. The ID token's signature and nonce aren't verified,
//! and the SIWC account isn't matched to the outer account.

use open_ferry_core::manager::CompletedTokenCommand;
use std::io;
use std::path::PathBuf;

use open_ferry_core::auth::codex_refresh::{generate_device_id, validate_device_id};
use open_ferry_core::auth::credential_lock::acquire_credential_lock;
use open_ferry_core::auth::file_store::BindIdentity;
use open_ferry_core::auth::{Auth, CodexCredentialPartition};
use open_ferry_core::config::AuthFile;
use open_ferry_providers::codex::siwc::SiwcAuth;
use open_ferry_providers::codex::token::SiwcTokenData;
use open_ferry_providers::oauth::{Pkce, generate_state};
use serde_json::Value;

use super::mutation_path::BindMutationPath;
use super::sessions::Lifeline;
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
    mutation_path: BindMutationPath,
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
    let mut matching = state
        .manager()
        .list()
        .into_iter()
        .filter(|auth| auth.id == name || auth.file_name == name);
    let registered = matching
        .next()
        .filter(|auth| eligible(auth))
        .filter(|_| matching.next().is_none())
        .ok_or_else(invalid)?;
    let command = state
        .manager()
        .capture_token_command(&registered.id, CodexCredentialPartition::Siwc)
        .ok_or_else(invalid)?;
    let registered = Auth::clone(&registered);
    let name = name.to_owned();
    let target = run_blocking(move || {
        let directory = files.base_dir().canonicalize()?;
        // Use the store's spelling for persistence and its credential lock;
        // canonical paths are only for containment and substitution checks.
        let path = files.file_path(&name)?;
        let real_path = directory.join(&name);
        let (metadata, file_id) = files.read_bind_target(&path)?;
        open_ferry_core::auth::file_store::check_bind_metadata(&registered.metadata, &metadata)?;
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
        if files.resolve_auth_path(&registered)?.canonicalize()? != real_path {
            return Err(invalid());
        }
        let mut record = registered.clone();
        record.metadata = metadata;
        Ok::<_, io::Error>(Target {
            path,
            mutation_path: BindMutationPath::bound(real_path.clone(), file_id),
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
        && ["access_token", "refresh_token"]
            .iter()
            .all(|key| auth.metadata_str(key).is_some_and(|v| !v.trim().is_empty()))
}

impl Target {
    pub(super) fn mutation_path(&self) -> BindMutationPath {
        self.mutation_path.clone()
    }
}

pub(super) async fn save(
    state: &ManagementState,
    mut target: Target,
    tokens: SiwcTokenData,
    session: String,
    registration: u64,
    lifeline: &mut Lifeline,
) -> io::Result<()> {
    if tokens.access_token.trim().is_empty()
        || tokens.refresh_token.trim().is_empty()
        || tokens.client_id.trim().is_empty()
    {
        return Err(io::Error::other("invalid SIWC bind token data"));
    }
    let store = state.credential_store().map_err(|_| invalid())?;
    let tokens = tokens.to_json();
    let mut waiting = None;
    loop {
        if state.oauth_sessions().is_shut_down()
            || !state.oauth_sessions().store().is_pending(&session, "codex")
        {
            return Err(invalid());
        }
        #[cfg(not(test))]
        let acquire = acquire_credential_lock(&target.path);
        #[cfg(test)]
        let acquire = async {
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
            .await
        };
        let guard = tokio::select! {
            biased;
            () = ended(state, &session, lifeline) => return Err(invalid()),
            acquired = acquire => acquired?,
        };
        let Ok(management_guard) = state.credential_lock().try_lock() else {
            drop(guard);
            tokio::select! {
                biased;
                () = ended(state, &session, lifeline) => return Err(invalid()),
                acquired = state.credential_lock().lock() => drop(acquired),
            }
            continue;
        };
        let files = store.files.clone();
        let checking = state.clone();
        let checking_session = session.clone();
        let tokens = tokens.clone();
        let (committed, returned, guard, returned_waiter) = run_blocking(move || {
            let commit = || {
                if files.base_dir().canonicalize()? != target.directory
                    || target.path.canonicalize()? != target.real_path
                {
                    return Err(invalid());
                }
                files.bind_siwc_locked(&target.record, tokens, &guard, &|disk, id| {
                    if files.base_dir().canonicalize()? != target.directory {
                        return Err(invalid());
                    }
                    target.identity.check(disk, id)?;
                    if checking.oauth_sessions().is_shut_down()
                        || !checking
                            .oauth_sessions()
                            .store()
                            .admit_bind(&checking_session, registration)
                    {
                        return Err(io::Error::other("SIWC bind session ended"));
                    }
                    Ok(())
                })
            };
            let committed = match waiting.as_mut() {
                Some(waiter) => open_ferry_core::manager::SiwcBindWaiter::try_commit(
                    waiter,
                    &target.registered,
                    commit,
                ),
                None => checking
                    .manager()
                    .try_commit_siwc_bind(&target.registered, commit),
            }?;
            // The exclusive barrier covers only validation and the durable write.
            let committed = committed
                .map(|record| {
                    let data = serde_json::to_vec(&record.metadata).map_err(|_| invalid())?;
                    Ok::<_, io::Error>((record, data))
                })
                .transpose()?;
            Ok::<_, io::Error>((committed, target, guard, waiting))
        })
        .await?;
        target = returned;
        waiting = returned_waiter;
        let Some((record, data)) = committed else {
            drop(guard);
            drop(management_guard);
            // Register writer preference only after releasing both locks.
            let waiter = waiting.get_or_insert_with(|| state.manager().queue_siwc_bind());
            #[cfg(test)]
            if let Some(signal) = state
                .oauth_sessions()
                .overrides()
                .bind_admission_wait
                .clone()
            {
                signal.notify_one();
            }
            loop {
                tokio::select! {
                    biased;
                    () = ended(state, &session, lifeline) => return Err(invalid()),
                    () = tokio::time::sleep(ADMISSION_BACKOFF) => {},
                }
                if waiter.is_ready() {
                    break;
                }
            }
            continue;
        };
        let file = AuthFile {
            path: target.path,
            data: data.into(),
        };
        let synced = match target.command.take() {
            Some(command) => store
                .sync
                .completed_token_command(record, Some(file), command),
            None => return Err(invalid()),
        };
        #[cfg(test)]
        let pause = state.oauth_sessions().overrides().bind_after_sync.clone();
        #[cfg(test)]
        if let Some(pause) = pause {
            pause.0.notify_one();
            pause.1.notified().await;
        }
        drop(guard);
        drop(management_guard);
        return synced
            .await
            .map_err(|_| io::Error::other("SIWC credential sync failed"));
    }
}

/// Bound admission polling, and session/owner cancellation while waiting for a
/// file lock, the management lock, or the manager's writer reservation.
const ADMISSION_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);

async fn ended(state: &ManagementState, session: &str, lifeline: &mut Lifeline) {
    loop {
        if state.oauth_sessions().is_shut_down()
            || !state.oauth_sessions().store().is_pending(session, "codex")
        {
            return;
        }
        tokio::select! {
            () = lifeline.cut() => return,
            () = tokio::time::sleep(ADMISSION_BACKOFF) => {},
        }
    }
}
