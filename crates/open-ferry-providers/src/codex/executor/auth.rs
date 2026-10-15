// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_auth.go
// (v8.0.27, MIT).
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
        // An explicit call with neither partition due can follow a 401 for an
        // unexpired token. Refresh every refreshable partition in that case.
        let explicit_unscheduled = !options.force && !outer_due && !siwc_due;
        let need_outer = has_outer && (options.force || explicit_unscheduled || outer_due);
        let need_siwc = has_siwc && (options.force || explicit_unscheduled || siwc_due);
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
