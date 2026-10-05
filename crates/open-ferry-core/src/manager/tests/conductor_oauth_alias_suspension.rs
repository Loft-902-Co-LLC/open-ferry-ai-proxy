// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_oauth_alias_suspension_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! An OAuth alias routes around a cooldown on the alias's own name: the
//! credential is picked for the upstream model the alias maps to.
//!
//! Deviations from upstream:
//! - The alias's `fork` flag isn't part of [`ModelAlias`]; the manager
//!   never reads it upstream either.
//! - Upstream's executor reads the requested model alias from the context
//!   (`RequestedModelAliasFromContext`); here it is the call options'
//!   `requested_model`, which the manager fills in from the route model.

use std::collections::BTreeMap;

use chrono::TimeDelta;

use super::support::*;
use crate::auth::{ModelState, Status};
use crate::exec::Dispatcher;
use crate::manager::{ModelAlias, Settings};

#[tokio::test(start_paused = true)]
async fn manager_execute_oauth_alias_bypasses_blocked_route_model() {
    const PROVIDER: &str = "antigravity";
    const ROUTE_MODEL: &str = "claude-opus-4-6";
    const TARGET_MODEL: &str = "claude-opus-4-6-thinking";

    let h = Harness::new(Settings {
        oauth_model_alias: BTreeMap::from([(
            PROVIDER.to_owned(),
            vec![ModelAlias {
                name: TARGET_MODEL.into(),
                alias: ROUTE_MODEL.into(),
                force_mapping: false,
            }],
        )]),
        ..Settings::default()
    });
    let executor = FakeExecutor::with(PROVIDER, |call: &Call| Reply::ok(call.model.clone()));
    h.executor(&executor);

    let mut auth = auth("oauth-alias-auth", PROVIDER);
    auth.status = Status::Active;
    auth.model_states.insert(
        ROUTE_MODEL.into(),
        ModelState {
            unavailable: true,
            status: Status::Error,
            next_retry_after: Some(h.now() + TimeDelta::hours(1)),
            ..ModelState::default()
        },
    );
    h.add(auth, &[ROUTE_MODEL, TARGET_MODEL]);

    let resp = h
        .manager
        .execute(&providers(&[PROVIDER]), request(ROUTE_MODEL), options())
        .await
        .expect("execute error, want success");
    assert_eq!(&resp.payload[..], TARGET_MODEL.as_bytes());

    assert_eq!(executor.models(Kind::Execute), [TARGET_MODEL]);

    let aliases: Vec<String> = executor
        .calls()
        .iter()
        .map(|call| call.options.metadata.requested_model.clone())
        .collect();
    assert_eq!(aliases, [ROUTE_MODEL]);
}
