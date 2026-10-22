// Ported from CLIProxyAPI internal/api/server_siwc_test.go (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use open_ferry_core::auth::Auth;

fn update_runtime(api: &Api, auth: Auth) {
    let id = auth.id.clone();
    // Inject each local status fixture through an unsaved live edit, retaining
    // the file store's purpose-specific preservation rules for ordinary saves.
    api.manager
        .edit_unsaved(&id, move |live| {
            live.metadata = auth.metadata;
            live.attributes = auth.attributes;
            Ok::<_, ()>(true)
        })
        .unwrap();
}

#[tokio::test]
async fn read_only_siwc_status_routes_selectors_and_summary() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let api = bind_api(&dir);
    let initial = api.manager.get("codex-bind.json").unwrap();
    let snapshot = std::fs::read(dir.path().join("codex-bind.json")).unwrap();
    let index = initial.index.clone();
    for route in ["oauth/siwc/status", "credentials/siwc/status"] {
        for query in [
            "name=codex-bind.json".to_owned(),
            "id=codex-bind.json".into(),
            format!("auth_index={index}"),
            format!("authIndex={index}"),
            format!("name=codex-bind.json&authIndex={index}"),
            format!("name=%20&id=codex-bind.json&auth_index=&authIndex={index}"),
        ] {
            assert_eq!(
                api.get(&format!("/v8/management/{route}?{query}"))
                    .await
                    .expect(StatusCode::OK),
                json!({"status":"off"})
            );
        }
        for (query, code) in [
            ("", StatusCode::BAD_REQUEST),
            ("?name=missing.json", StatusCode::NOT_FOUND),
            (
                "?name=codex-bind.json&auth_index=wrong",
                StatusCode::NOT_FOUND,
            ),
        ] {
            api.get(&format!("/v8/management/{route}{query}"))
                .await
                .expect(code);
        }
    }
    for (siwc, expected) in [
        (
            json!({"access_token":"nested", "expires_at":"2099-01-01T00:00:00Z", "expired":"2000-01-01T00:00:00Z"}),
            "valid",
        ),
        (
            json!({"access_token":"nested", "expires_at":"2000-01-01T00:00:00Z"}),
            "invalid",
        ),
        (
            json!({"access_token":"nested", "expires_in":3600,"last_refresh":"2000-01-01T00:00:00Z"}),
            "invalid",
        ),
        (json!({"access_token":"e30.eyJleHAiOjF9.sig"}), "invalid"),
        (json!({"expires_at":"2099-01-01T00:00:00Z"}), "invalid"),
    ] {
        let mut auth = Auth::clone(&api.manager.get("codex-bind.json").unwrap());
        auth.metadata.insert("use_siwc".into(), json!(true));
        auth.metadata.insert("siwc".into(), siwc);
        auth.attributes.insert("runtime_only".into(), "true".into());
        update_runtime(&api, auth);
        assert_eq!(
            api.get("/v8/management/oauth/siwc/status?name=codex-bind.json")
                .await
                .expect(StatusCode::OK),
            json!({"status":expected})
        );
        let entries = api
            .get("/v8/management/credentials?name=codex-bind.json")
            .await
            .expect(StatusCode::OK);
        assert_eq!(entries["files"][0]["siwc_status"], expected);
    }
    let mut auth = Auth::clone(&api.manager.get("codex-bind.json").unwrap());
    auth.metadata["siwc"] =
        json!({"access_token":"rejected-nested", "expires_at":"2099-01-01T00:00:00Z"});
    update_runtime(&api, auth);
    api.manager
        .mark_result(&open_ferry_core::manager::CallResult {
            auth_id: "codex-bind.json".into(),
            access_token: "rejected-nested".into(),
            error: Some(open_ferry_core::auth::AuthError {
                http_status: 401,
                ..Default::default()
            }),
            ..Default::default()
        });
    assert_eq!(
        api.get("/v8/management/credentials/siwc/status?name=codex-bind.json")
            .await
            .expect(StatusCode::OK),
        json!({"status":"invalid"})
    );
    let mut auth = Auth::clone(&api.manager.get("codex-bind.json").unwrap());
    auth.metadata["siwc"]["access_token"] = json!("changed-nested");
    update_runtime(&api, auth);
    assert_eq!(
        api.get("/v8/management/credentials/siwc/status?name=codex-bind.json")
            .await
            .expect(StatusCode::OK),
        json!({"status":"valid"})
    );
    let other = Auth {
        id: "other".into(),
        provider: "claude".into(),
        ..Default::default()
    };
    api.register(other);
    api.get("/v8/management/oauth/siwc/status?id=other")
        .await
        .expect(StatusCode::BAD_REQUEST);
    assert_eq!(
        std::fs::read(dir.path().join("codex-bind.json")).unwrap(),
        snapshot
    );
    assert_eq!(
        api.manager
            .get("codex-bind.json")
            .unwrap()
            .registration_epoch,
        initial.registration_epoch
    );
}

#[tokio::test]
async fn status_id_precedes_file_name_and_shared_index_lookup() {
    let dir = AuthDir::new();
    let api = bind_api(&dir);
    for (id, file_name, enabled) in [
        ("a/collision.json", "collision.json", false),
        ("collision.json", "other.json", true),
    ] {
        let mut auth = Auth {
            id: id.into(),
            file_name: file_name.into(),
            provider: "codex".into(),
            ..Default::default()
        };
        auth.metadata = json!({"type":"codex", "use_siwc":enabled, "siwc":{"access_token":"dummy", "expires_at":"2099-01-01T00:00:00Z"}}).as_object().unwrap().clone();
        api.register(auth);
    }
    assert_eq!(
        api.get("/v8/management/oauth/siwc/status?name=collision.json")
            .await
            .expect(StatusCode::OK),
        json!({"status":"valid"})
    );
    let index = api.manager.get("a/collision.json").unwrap().index.clone();
    for query in [
        format!("auth_index={index}"),
        format!("name=collision.json&auth_index={index}"),
    ] {
        assert_eq!(
            api.get(&format!("/v8/management/oauth/siwc/status?{query}"))
                .await
                .expect(StatusCode::OK),
            json!({"status":"off"})
        );
    }
}
#[tokio::test]
async fn legacy_credential_summary_omits_siwc_status() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let api = bind_api(&dir);
    let listing = api
        .get("/v0/management/auth-files")
        .await
        .expect(StatusCode::OK);
    assert!(listing["files"][0].get("siwc_status").is_none());
}

#[tokio::test]
async fn status_future_sources_and_precedence() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let api = bind_api(&dir);
    let future_jwt = "e30.eyJleHAiOjQwNzA5MDg4MDB9.sig";
    let past_jwt = "e30.eyJleHAiOjF9.sig";
    let mut cases = vec![
        (json!({"expires_at":"2099-01-01T00:00:00Z"}), "valid"),
        (json!({"expiresAt":"2099-01-01T00:00:00Z"}), "valid"),
        (json!({"access_token":future_jwt}), "valid"),
        (
            json!({"expires_at":"2000-01-01T00:00:00Z", "access_token":future_jwt}),
            "invalid",
        ),
        (
            json!({"expires_at":"2099-01-01T00:00:00Z", "access_token":past_jwt}),
            "valid",
        ),
        (
            json!({"expires_in":1, "issued_at":"2000-01-01T00:00:00Z", "access_token":future_jwt}),
            "invalid",
        ),
        (
            json!({"expires_in":3600, "issued_at":"2099-01-01T00:00:00Z", "access_token":past_jwt}),
            "valid",
        ),
    ];
    for lifetime in ["expires_in", "expiresIn"] {
        for issuance in [
            "last_refresh",
            "lastRefresh",
            "timestamp",
            "issued_at",
            "issuedAt",
        ] {
            let mut fields = serde_json::Map::new();
            fields.insert(lifetime.into(), json!(3600));
            fields.insert(issuance.into(), json!("2099-01-01T00:00:00Z"));
            cases.push((serde_json::Value::Object(fields), "valid"));
        }
    }
    for (fields, expected) in cases {
        let mut auth = Auth::clone(&api.manager.get("codex-bind.json").unwrap());
        auth.metadata.insert("use_siwc".into(), json!(true));
        auth.metadata
            .insert("siwc".into(), json!({"access_token":"dummy-nested"}));
        auth.metadata["siwc"]
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        update_runtime(&api, auth);
        assert_eq!(
            api.get("/v8/management/oauth/siwc/status?name=codex-bind.json")
                .await
                .expect(StatusCode::OK),
            json!({"status":expected}),
            "source: {fields}"
        );
    }
}
