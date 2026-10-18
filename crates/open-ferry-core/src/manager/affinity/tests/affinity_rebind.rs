// Ported from CLIProxyAPI sdk/cliproxy/auth/selector.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Regression coverage for quota-only alias and subagent failover branches.

use std::collections::HashSet;
use std::sync::Arc;

use super::base;
use crate::auth::Auth;
use crate::manager::Settings;
use crate::manager::affinity::{Affinity, Session};

#[test]
fn affinity_rebind_quota_preserves_alias_but_isolates_subagents_and_forks() {
    for (primary, fallback, is_fork, preserve) in [
        ("prompt-cache:alias", "conversation:parent", false, true),
        ("header:parent:agent:child", "header:parent", false, false),
        ("header:fork", "conversation:parent", true, false),
    ] {
        for subagents in [false, true] {
            let mut affinity = Affinity::for_settings(&Settings {
                session_affinity: true,
                session_affinity_rebind_on: "quota".into(),
                session_affinity_subagents: Some(subagents),
                ..Settings::default()
            })
            .unwrap();
            let parent_key = format!("mixed::{fallback}::model");
            affinity.cache.set(&parent_key, "high", base());
            let sess = Session {
                primary: primary.into(),
                fallback: fallback.into(),
                is_fork,
            };
            let low = Arc::new(Auth {
                id: "low".into(),
                ..Auth::default()
            });
            let available = [&low];
            let transient = HashSet::from(["high".into()]);
            let selected = affinity
                .pick(
                    "mixed",
                    "model",
                    Some(&sess),
                    (&available, &transient),
                    base(),
                    |tier| Ok(tier[0]),
                )
                .unwrap();
            assert_eq!(selected.id, "low");
            let primary_key = format!("mixed::{primary}::model");
            assert_eq!(
                affinity.bound(&primary_key, base()).as_deref(),
                Some(if preserve { "high" } else { "low" })
            );
            assert_eq!(affinity.bound(&parent_key, base()).as_deref(), Some("high"));
            // A child with its own binding preserves it across transient failures,
            // independently of whether parent inheritance is enabled.
            if !preserve {
                let transient = HashSet::from(["low".into()]);
                let high = Arc::new(Auth {
                    id: "high".into(),
                    ..Auth::default()
                });
                let available = [&high];
                let selected = affinity
                    .pick(
                        "mixed",
                        "model",
                        Some(&sess),
                        (&available, &transient),
                        base(),
                        |tier| Ok(tier[0]),
                    )
                    .unwrap();
                assert_eq!(selected.id, "high");
                assert_eq!(affinity.bound(&primary_key, base()).as_deref(), Some("low"));
            }
        }
    }
}
