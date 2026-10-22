// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_auth.go
// (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Independent token rotations. No credential files are touched here: the
//! manager owns refresh-then-save in a cancellation-safe gate operation and
//! applies the baseline-checked patch. Cross-process refresh serialization
//! needs that operation to hold the credential lock through transport/save.

use std::sync::Arc;

use chrono::Utc;
use open_ferry_core::auth::codex_refresh::{CodexPersistIntent, CredentialRefreshPatch};
use open_ferry_core::auth::{Auth, AuthKind, CodexCredentialPartition, CodexRefreshFailure};
use open_ferry_core::exec::{ErrorKind, ExecError};
use open_ferry_core::executor::RefreshOptions;
use serde_json::Value;

use super::{CodexExecutor, REFRESH_ATTEMPTS, REFRESH_LEAD};
use crate::codex::siwc::SiwcAuth;
use crate::codex::token::SiwcTokenData;

/// Which partitions to send a refresh for, as `(outer, siwc)`. Each input pair
/// is `(refreshable, due)`, computed on the record the executor was given.
pub(super) fn refresh_decision(
    options: RefreshOptions,
    (has_outer, outer_due): (bool, bool),
    (has_siwc, siwc_due): (bool, bool),
) -> (bool, bool) {
    // `was_due` is the manager's reading of the record before it adopted
    // anything; without adoption it is the due state of this record.
    let due_before = options
        .adopted
        .map_or(outer_due || siwc_due, |adopted| adopted.was_due);
    // An explicit call with neither partition due can follow a 401 for an
    // unexpired token. Refresh every refreshable partition in that case,
    // except one the manager adopted from another process.
    let explicit_unscheduled = !options.force && !due_before;
    let adopted_outer = options.adopted.is_some_and(|a| a.outer);
    let adopted_siwc = options.adopted.is_some_and(|a| a.siwc);
    (
        has_outer && (options.force || outer_due || (explicit_unscheduled && !adopted_outer)),
        has_siwc && (options.force || siwc_due || (explicit_unscheduled && !adopted_siwc)),
    )
}

impl CodexExecutor {
    pub(super) async fn refresh_inner(
        &self,
        auth: Arc<Auth>,
        options: RefreshOptions,
    ) -> Result<Auth, ExecError> {
        let now = Utc::now();
        let stored_siwc = auth.metadata.get("siwc").and_then(Value::as_object);
        let has_siwc = !auth.siwc_token_field("refresh_token").is_empty()
            && !auth.siwc_token_field("client_id").is_empty();
        let has_outer = !auth
            .metadata_str("refresh_token")
            .unwrap_or_default()
            .trim()
            .is_empty();
        let outer_due = has_outer
            && auth
                .outer_refresh_due_at(now, Some(REFRESH_LEAD))
                .is_some_and(|due| due <= now);
        let siwc_due = has_siwc && auth.siwc_refresh_due_at(now).is_some_and(|due| due <= now);
        let (need_outer, need_siwc) =
            refresh_decision(options, (has_outer, outer_due), (has_siwc, siwc_due));
        let adopted_any = options.adopted.is_some_and(|a| a.outer || a.siwc);
        let mut refreshed = (*auth).clone();
        let mut outer_rotated = false;
        let mut siwc_rotated = false;
        let mut failure = None;
        if need_outer {
            match self.refresh_outer(auth.clone()).await {
                Ok(after) => {
                    refreshed = after;
                    outer_rotated = true;
                }
                Err(error) => {
                    failure = Some(CodexRefreshFailure {
                        error,
                        partition: CodexCredentialPartition::Outer,
                    });
                }
            }
        }
        if need_siwc {
            let result = async {
                let previous =
                    SiwcTokenData::from_json(stored_siwc.expect("refreshable SIWC is an object"))
                        .map_err(|message| ExecError::new(ErrorKind::Upstream, message))?;
                SiwcAuth::with_proxy_url(&self.proxy_for(&auth))
                    .with_endpoints(self.siwc_endpoints.clone())
                    .refresh_tokens_with_retry(&previous, REFRESH_ATTEMPTS)
                    .await
                    .map_err(|error| {
                        ExecError::new(ErrorKind::Upstream, error.message())
                            .with_status(error.status())
                    })
            }
            .await;
            match result {
                Ok(tokens) => {
                    let mut merged = stored_siwc.cloned().unwrap_or_default();
                    merged.remove("expires_at");
                    merged.extend(tokens.to_json());
                    refreshed
                        .metadata
                        .insert("siwc".into(), Value::Object(merged));
                    siwc_rotated = true;
                }
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(CodexRefreshFailure {
                            error,
                            partition: CodexCredentialPartition::Siwc,
                        });
                    }
                }
            }
        }
        if !outer_rotated && !siwc_rotated {
            if let Some(failure) = failure {
                return Err(failure.error);
            }
            // The manager adopted a partition another process already rotated
            // and published, so there is nothing left to send or persist.
            if adopted_any {
                return Ok(refreshed);
            }
            if auth.auth_kind() == Some(AuthKind::ApiKey) {
                return Ok(refreshed);
            }
            // OAuth success here would reschedule every 30 seconds and clear
            // the revoked token's 401 cooldowns without rotating its token.
            return Err(ExecError::new(
                ErrorKind::Upstream,
                "codex executor: no refreshable token",
            ));
        }
        refreshed.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &auth.metadata,
            &refreshed.metadata,
            outer_rotated,
            siwc_rotated,
        ));
        refreshed.codex_refresh_failure = failure;
        Ok(refreshed)
    }
}
