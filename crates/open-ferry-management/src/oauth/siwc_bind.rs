// Ported from CLIProxyAPI internal/cmd/siwc_bind.go and
// internal/api/handlers/management/oauth_siwc.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Management binding uses the same leaf-link refusal and save-time identity
//! check as the CLI flow. The ID token's signature and nonce aren't verified,
//! and the SIWC account isn't matched to the outer account.

use open_ferry_core::manager::CompletedTokenCommand;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use open_ferry_core::auth::codex_refresh::{generate_device_id, validate_device_id};
use open_ferry_core::auth::credential_lock::try_acquire_credential_lock;
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
) -> io::Result<SaveOutcome> {
    use open_ferry_core::manager::{SIWC_BIND_DRAIN_TIMEOUT, SiwcBindDrain};
    if tokens.access_token.trim().is_empty()
        || tokens.refresh_token.trim().is_empty()
        || tokens.client_id.trim().is_empty()
    {
        return Err(invalid());
    }
    let store = state.credential_store().map_err(|_| invalid())?;
    let files = store.files.clone();
    let tokens = tokens.to_json();
    let cancellation = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let cancelled = cancellation.0.clone();
    let checking = state.clone();
    let checking_session = session.clone();
    // A dedicated worker keeps its FIFO place without occupying a pool slot.
    // Nothing reserved or thread-confined crosses this future's awaits.
    let mut work = Box::pin(run_bind_worker(state, move || {
        let manager = checking.manager();
        let turn = manager.siwc_bind_turn(&target.registered.id);
        let mut failures = 0u32;
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(invalid());
            }
            let mut file_busy = false;
            let mut attempted = false;
            let attempt = (|| -> io::Result<Option<Saved>> {
                let Some(mut reservation) = turn.reserve_siwc_bind() else {
                    return Ok(None);
                };
                attempted = true;
                #[cfg(test)]
                if let Some(times) = checking
                    .oauth_sessions()
                    .overrides()
                    .bind_admission_times
                    .clone()
                {
                    times.lock().unwrap().push(std::time::Instant::now());
                }
                #[cfg(test)]
                if let Some(signal) = checking
                    .oauth_sessions()
                    .overrides()
                    .bind_admission_wait
                    .clone()
                {
                    signal.notify_one();
                }
                let budget = SIWC_BIND_DRAIN_TIMEOUT;
                #[cfg(test)]
                let budget = checking
                    .oauth_sessions()
                    .overrides()
                    .bind_drain_budget
                    .unwrap_or(budget);
                match reservation.wait_drained(&cancelled, budget) {
                    SiwcBindDrain::Ready => {}
                    SiwcBindDrain::TimedOut | SiwcBindDrain::Interrupted => return Ok(None),
                }
                #[cfg(test)]
                let before_file = checking
                    .oauth_sessions()
                    .overrides()
                    .bind_before_file
                    .clone();
                #[cfg(test)]
                if let Some(pause) = before_file {
                    pause.0.notify_one();
                    pause
                        .1
                        .lock()
                        .unwrap()
                        .recv_timeout(std::time::Duration::from_secs(30))
                        .unwrap();
                }
                let Some(guard) = try_acquire_credential_lock(&target.path)? else {
                    file_busy = true;
                    #[cfg(test)]
                    if let Some(signal) =
                        checking.oauth_sessions().overrides().bind_lock_wait.clone()
                    {
                        signal.notify_one();
                    }
                    return Ok(None);
                };
                let Some(management_guard) = checking.try_credential_lock() else {
                    return Ok(None);
                };
                let commit = || {
                    if files.base_dir().canonicalize()? != target.directory
                        || target.path.canonicalize()? != target.real_path
                    {
                        return Err(invalid());
                    }
                    files.try_bind_siwc_locked(
                        &target.record,
                        tokens.clone(),
                        &guard,
                        &|disk, id| {
                            if files.base_dir().canonicalize()? != target.directory {
                                return Err(invalid());
                            }
                            target.identity.check(disk, id)?;
                            if cancelled.load(Ordering::Acquire)
                                || !checking
                                    .oauth_sessions()
                                    .admit_bind(&checking_session, registration)
                            {
                                return Err(io::Error::other("SIWC bind session ended"));
                            }
                            #[cfg(test)]
                            {
                                let pause = checking
                                    .oauth_sessions()
                                    .overrides()
                                    .bind_before_write
                                    .clone();
                                if let Some(pause) = pause {
                                    pause.0.notify_one();
                                    pause
                                        .1
                                        .lock()
                                        .unwrap()
                                        .recv_timeout(std::time::Duration::from_secs(30))
                                        .unwrap();
                                }
                            }
                            Ok(())
                        },
                    )
                };
                let committed = reservation
                    .try_commit(&target.registered, &cancelled, commit)?
                    .flatten();
                drop(reservation);
                match committed {
                    Some(record) => {
                        let data = serde_json::to_vec(&record.metadata).map_err(|_| invalid())?;
                        Ok(Some((record, data, guard, management_guard)))
                    }
                    None => Ok(None),
                }
            })()?;
            if let Some(saved) = attempt {
                return Ok((saved, target));
            }
            if attempted {
                failures = failures.saturating_add(1);
            }
            let jitter = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
                % 21;
            let backoff =
                retry_delay(attempted, failures) + std::time::Duration::from_millis(u64::from(jitter));
            if file_busy {
                turn.defer_file_retry(backoff);
            }
            if !turn.wait_retry(backoff, &cancelled) {
                return Err(invalid());
            }
        }
    }));
    let ((record, data, guard, management_guard), returned) = tokio::select! {
        result = &mut work => result?,
        () = ended(state, &session, lifeline) => {
            // A signal cancels admission, but cannot turn an already durable
            // write into SAVE_FAILED or skip its completed-token command.
            drop(cancellation);
            work.await?
        }
    };
    target = returned;
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
    Ok(match synced.await {
        Ok(()) => SaveOutcome::Synchronized,
        Err(_) => SaveOutcome::RuntimeSyncUnavailable,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SaveOutcome {
    Synchronized,
    RuntimeSyncUnavailable,
}

// Capacity denial is retried promptly even after earlier transaction misses.
fn retry_delay(attempted: bool, failures: u32) -> std::time::Duration {
    if attempted {
        drain_backoff(failures)
    } else {
        ADMISSION_BACKOFF
    }
}

fn drain_backoff(timeouts: u32) -> std::time::Duration {
    std::time::Duration::from_millis((20u64 << timeouts.saturating_sub(1).min(6)).min(1000))
}

/// Operation-scoped dedicated worker: a retained FIFO turn must not consume
/// the blocking pool needed by detached saves holding shared admission.
async fn run_bind_worker<T: Send + 'static>(
    _state: &ManagementState,
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    // Refuse immediately before spawning, without a turn or reservation.
    // Session cancellation cannot return this permit: only worker exit does.
    let permit = _state.oauth_sessions().try_bind_worker()?;
    let operation = open_ferry_core::gate::Operation::current();
    #[cfg(test)]
    let finished = _state
        .oauth_sessions()
        .overrides()
        .bind_worker_finished
        .clone();
    #[cfg(test)]
    let starts = _state
        .oauth_sessions()
        .overrides()
        .bind_worker_starts
        .clone();
    let (send, receive) = tokio::sync::oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("siwc-bind".into())
        .spawn(move || {
            let _permit = permit;
            #[cfg(test)]
            if let Some(starts) = starts {
                starts.fetch_add(1, Ordering::AcqRel);
            }
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match operation {
                    Some(operation) => operation.scope_blocking(work),
                    None => work(),
                }));
            #[cfg(test)]
            if let Some(finished) = finished {
                finished.notify_one();
            }
            let _ = send.send(result);
        })?;
    drop(thread);
    match receive.await {
        Ok(Ok(result)) => result,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => Err(io::Error::other("SIWC bind worker ended without a result")),
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

type Saved = (
    Auth,
    Vec<u8>,
    open_ferry_core::auth::credential_lock::CredentialLock,
    tokio::sync::OwnedMutexGuard<()>,
);

#[cfg(test)]
pub(crate) async fn save_for_test(state: &ManagementState, session: &str) -> io::Result<()> {
    let bind = prepare(state, "codex-bind.json").await?;
    let mut registration = state
        .oauth_sessions()
        .store()
        .register(session, "codex")
        .unwrap();
    assert!(state.oauth_sessions().store().mark_bind_target(
        session,
        bind.target.mutation_path(),
        "http://localhost:1455/auth/callback"
    ));
    save(
        state,
        bind.target,
        SiwcTokenData {
            access_token: "round3-test".into(),
            refresh_token: "round3-test-refresh".into(),
            client_id: "round3-client".into(),
            ..Default::default()
        },
        session.into(),
        registration.id,
        &mut registration.lifeline,
    )
    .await
    .map(|_| ())
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Poll session/owner end while no reservation exists on the async caller.
/// Reserved drain waits have their own independent 100 ms thread deadline.
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

#[cfg(test)]
mod round3_tests {
    use super::*;
    #[test]
    fn round5_unreserved_retries_use_the_base_delay() {
        for failures in 0..100 {
            assert_eq!(retry_delay(false, failures), ADMISSION_BACKOFF);
            assert_eq!(retry_delay(true, failures), drain_backoff(failures));
        }
    }

    #[test]
    fn round3_drain_timeouts_grow_backoff_and_cap_at_one_second() {
        let actual: Vec<_> = (0..10)
            .map(|count| drain_backoff(count).as_millis())
            .collect();
        assert_eq!(actual, [20, 20, 40, 80, 160, 320, 640, 1000, 1000, 1000]);
    }
}
