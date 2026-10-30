// Ports upstream 4138edb1's configured-401 matrix, with partition and stale-result fences.
use super::support::*;
use crate::auth::{Auth, AuthError};
use crate::exec::{Dispatcher, ExecError};
use crate::manager::classify::{CODE_FORCE_COOLDOWN, CODE_REQUEST_SCOPED};
use crate::manager::{CallResult, RequestScopedErrorRule, Settings};
use serde_json::json;
use std::collections::BTreeMap;

fn credential(id: &str, refreshable: bool, siwc: bool) -> Auth {
    let mut auth = auth_with_metadata(
        id,
        "codex",
        json!({
            "type":"codex", "access_token":"outer-dummy", "use_siwc":siwc,
            "siwc":{"access_token":"nested-dummy", "client_id":"dummy"}
        }),
    );
    if refreshable {
        auth.metadata
            .insert("refresh_token".into(), json!("refresh-dummy"));
    }
    auth.attributes.insert("auth_kind".into(), "oauth".into());
    auth.attributes.insert(
        "priority".into(),
        if id == "primary" { "10" } else { "0" }.into(),
    );
    auth
}

#[tokio::test(start_paused = true)]
async fn configured_401_matrix_never_rejects_or_refreshes_and_cools_only_when_requested() {
    for action in [
        "stop",
        "continue",
        "stop-and-cooldown",
        "continue-and-cooldown",
    ] {
        for refreshable in [false, true] {
            for disabled_cooling in [false, true] {
                for siwc in [false, true] {
                    for phase in ["execute", "count", "open", "bootstrap", "tail"] {
                        let h = Harness::new(Settings {
                            disable_cooling: disabled_cooling,
                            oauth_request_scoped_errors: BTreeMap::from([(
                                "codex".into(),
                                vec![RequestScopedErrorRule {
                                    status: 401,
                                    matches: vec!["marker".into()],
                                    action: action.into(),
                                    ..Default::default()
                                }],
                            )]),
                            ..Default::default()
                        });
                        let executor = FakeExecutor::with("codex", move |call| {
                            if call.auth_id != "primary" {
                                return Reply::ok("backup");
                            }
                            match phase {
                                "bootstrap" => {
                                    Reply::chunks(vec![Err(ExecError::upstream(401, "marker"))])
                                }
                                "tail" => Reply::chunks(vec![
                                    Ok("first".into()),
                                    Err(ExecError::upstream(401, "marker")),
                                ]),
                                _ => Reply::status(401, "marker"),
                            }
                        });
                        h.executor(&executor);
                        h.add(credential("primary", refreshable, siwc), &["m"]);
                        h.add(credential("backup", false, siwc), &["m"]);
                        let stopped = match phase {
                            "execute" => h
                                .manager
                                .execute(&providers(&["codex"]), request("m"), options())
                                .await
                                .is_err(),
                            "count" => h
                                .manager
                                .count_tokens(&providers(&["codex"]), request("m"), options())
                                .await
                                .is_err(),
                            _ => match h
                                .manager
                                .execute_stream(&providers(&["codex"]), request("m"), options())
                                .await
                            {
                                Err(_) => true,
                                Ok(stream) => collect(stream).await.1.is_some(),
                            },
                        };
                        assert_eq!(
                            stopped,
                            action.starts_with("stop") || phase == "tail",
                            "{action}/{phase}"
                        );
                        let calls = executor.calls();
                        assert_eq!(calls.iter().filter(|c| c.auth_id == "primary").count(), 1);
                        assert_eq!(executor.refresh_count(), 0, "{action}/{phase}");
                        let live = h.get("primary");
                        assert!(live.rejected_access_token.is_empty(), "{action}/{phase}");
                        assert!(
                            live.rejected_siwc_access_token.is_empty(),
                            "{action}/{phase}"
                        );
                        assert_eq!(live.next_refresh_after, None, "{action}/{phase}");
                        assert_eq!(
                            live.unavailable,
                            action.ends_with("and-cooldown"),
                            "{action}/{phase}/{disabled_cooling}"
                        );
                        assert_eq!(
                            live.next_retry_after.is_some(),
                            action.ends_with("and-cooldown")
                        );
                        if !action.ends_with("and-cooldown") {
                            let _ = h
                                .manager
                                .execute(&providers(&["codex"]), request("m"), pinned("primary"))
                                .await;
                            assert_eq!(
                                executor
                                    .calls()
                                    .iter()
                                    .filter(|c| c.auth_id == "primary")
                                    .count(),
                                2,
                                "an access-only credential remains selectable after {action}/{phase}"
                            );
                            assert_eq!(executor.refresh_count(), 0);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn stale_configured_401_results_do_not_reject_a_sibling_partition() {
    for code in [CODE_FORCE_COOLDOWN, CODE_REQUEST_SCOPED, ""] {
        let h = Harness::new(Default::default());
        let initial = h.add(credential("primary", true, true), &["m"]);
        let mut switched = initial.as_ref().clone();
        switched.metadata["use_siwc"] = json!(false);
        switched.metadata["access_token"] = json!("new-outer");
        h.manager.update_unsaved(switched).unwrap();
        let before = h.get("primary");
        h.manager.mark_result(&CallResult {
            auth_id: "primary".into(),
            provider: "codex".into(),
            model: "m".into(),
            access_token: "nested-dummy".into(),
            credential_version: initial.credential_version,
            registration_epoch: initial.registration_epoch,
            error: Some(AuthError {
                code: code.into(),
                http_status: 401,
                message: "marker".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        let live = h.get("primary");
        assert_eq!(!live.rejected_siwc_access_token.is_empty(), code.is_empty());
        assert!(live.rejected_access_token.is_empty());
        assert_eq!(live.failed, before.failed, "stale results are not counted");
        assert_eq!(live.next_refresh_after, before.next_refresh_after);
    }
}

#[tokio::test(start_paused = true)]
async fn unmatched_401_still_rejects_the_selected_bearer() {
    for siwc in [false, true] {
        let h = Harness::new(Default::default());
        let executor = FakeExecutor::with("codex", |_| Reply::status(401, "real token rejection"));
        h.executor(&executor);
        h.add(credential("primary", false, siwc), &["m"]);
        assert!(
            h.manager
                .execute(&providers(&["codex"]), request("m"), options())
                .await
                .is_err()
        );
        assert!(!h.get("primary").rejected_access_token.is_empty());
        assert_eq!(
            !h.get("primary").rejected_siwc_access_token.is_empty(),
            siwc
        );
    }
}
