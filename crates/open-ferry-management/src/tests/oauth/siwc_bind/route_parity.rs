//! Local route comparisons with upstream's management tests. Only provider
//! endpoints are mocked, and generated state/PKCE/device values are not emitted.

use super::*;

#[tokio::test]
async fn callback_accepts_empty_fragment_and_refuses_nonempty_fragment() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    api.state.oauth_sessions().overrides().ports = Some((1455, 1455));
    let session = start_login(&api, START).await;
    let callback =
        format!("http://127.0.0.1:1455/auth/callback?state={session}&code=dummy&client_id=dummy");
    api.post(
        "/v8/management/oauth/callback",
        &json!({"redirect_url":format!("{callback}#refused")}).to_string(),
    )
    .await
    .expect(StatusCode::BAD_REQUEST);
    assert!(upstream.requests().is_empty());
    api.post(
        "/v8/management/oauth/callback",
        &json!({"redirect_url":format!("{callback}#")}).to_string(),
    )
    .await
    .expect(StatusCode::OK);
    completed(&api, &session).await;
    assert_eq!(upstream.requests().len(), 1);
}

#[tokio::test]
async fn siwc_route_parity_records() {
    let mut rows = Vec::new();
    {
        let dir = AuthDir::new();
        dir.write("codex-bind.json", OUTER);
        let api = bind_api(&dir);
        for (id, body) in [
            ("bind_contract", r#"{"name":"codex-bind.json"}"#),
            ("bind_duplicate", r#"{"name":"codex-bind.json"}"#),
            ("bind_missing_name", "{}"),
            ("bind_bad_type", r#"{"name":4}"#),
            ("bind_traversal", r#"{"name":"../x.json"}"#),
            ("bind_missing_file", r#"{"name":"missing.json"}"#),
        ] {
            let response = api.post("/v8/management/oauth/siwc/bind", body).await;
            if id == "bind_contract" {
                let status = response.status.as_u16();
                let body = response.expect(StatusCode::OK);
                rows.push(json!({"id":id,"status":status,"flow":body["flow"],"expires_in":body["expires_in"]}));
            } else {
                assert_eq!(
                    response.status,
                    if id == "bind_duplicate" {
                        StatusCode::CONFLICT
                    } else if matches!(id, "bind_traversal" | "bind_missing_file") {
                        StatusCode::NOT_FOUND
                    } else {
                        StatusCode::BAD_REQUEST
                    }
                );
                rows.push(json!({"id":id,"status":response.status.as_u16()}));
            }
        }
    }
    for id in [
        "exact",
        "empty_fragment",
        "nonempty_fragment",
        "dot_path",
        "numeric_host",
        "userinfo",
        "duplicate_client",
        "conflicting_state",
        "unknown_body_state",
        "legacy",
        "get_callback",
    ] {
        let dir = AuthDir::new();
        dir.write("codex-bind.json", OUTER);
        let upstream = Upstream::answering(tokens()).await;
        let api = login_api(&dir, &upstream.url);
        api.state.oauth_sessions().overrides().ports = Some((1455, 1455));
        let session = start_login(&api, START).await;
        let query = format!("state={session}&code=dummy&client_id=dummy");
        let base = format!("http://127.0.0.1:1455/auth/callback?{query}");
        let url = match id {
            "empty_fragment" => format!("{base}#"),
            "nonempty_fragment" => format!("{base}#refused"),
            "dot_path" => base.replace("/auth/callback", "/other/../auth/callback"),
            "numeric_host" => base.replace("127.0.0.1", "2130706433"),
            "userinfo" => base.replace("://", "://@"),
            "duplicate_client" => format!("{base}&client_id=other"),
            _ => base,
        };
        let mut body = json!({"redirect_url":url});
        if id == "conflicting_state" {
            body["redirect_url"] = json!(
                body["redirect_url"]
                    .as_str()
                    .unwrap()
                    .replace(&session, "other")
            );
            body["state"] = json!(session);
        } else if id == "unknown_body_state" {
            body["state"] = json!("other");
        }
        let response = match id {
            "legacy" => {
                api.post("/v0/management/oauth-callback", &body.to_string())
                    .await
            }
            "get_callback" => {
                api.get(&format!("/v8/management/oauth/callback?{query}"))
                    .await
            }
            _ => {
                api.post("/v8/management/oauth/callback", &body.to_string())
                    .await
            }
        };
        let status = response.status.as_u16();
        let accepted = matches!(id, "exact" | "empty_fragment" | "get_callback");
        assert_eq!(
            status,
            if accepted {
                200
            } else if id == "legacy" {
                409
            } else if id == "unknown_body_state" {
                404
            } else {
                400
            },
            "{id}"
        );
        if accepted {
            completed(&api, &session).await;
        }
        rows.push(json!({"id":id,"status":status,"pending":api.state.oauth_sessions().store().is_pending(&session,"codex"),"saved":dir.read_json("codex-bind.json")["use_siwc"]==true,"exchanges":upstream.requests().len()}));
    }
    for (id, obsolete) in [
        ("current_alias_matches", false),
        ("obsolete_alias_matches", true),
    ] {
        let matched = super::mutation_identity::refreshed_alias_invalidation(obsolete, false).await;
        rows.push(json!({"id":id,"matches":matched}));
    }
    assert_eq!(rows.len(), 19);
    if let Some(path) = std::env::var_os("P36_SIWC_RUST_PARITY") {
        std::fs::write(path, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
    }
}
