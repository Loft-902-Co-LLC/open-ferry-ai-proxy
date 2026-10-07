//! `GET /claude-cli/entries`, with credentials made of the entries as the
//! server makes them; and `GET /claude-cli/auth-status`, with a script
//! standing in for Claude Code, which prints what `claude auth status
//! --json` prints.

use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use http::{Method, StatusCode};
use open_ferry_core::auth::synthesizer::{
    StableIdGenerator, SynthesisContext, synthesize_config_auths,
};
use open_ferry_core::auth::{Auth, AuthError, Status};
use open_ferry_core::config::{ClaudeCli, ClaudeKey, Config};
use serde_json::{Value, json};

use super::{Dash, LOCAL, keyed_config, request};

const ENTRIES: &str = "/open-ferry/api/v1/claude-cli/entries";
const STATUS: &str = "/open-ferry/api/v1/claude-cli/auth-status";

/// What Claude Code prints for a signed-in account: more than the route
/// passes on.
const SIGNED_IN: &str = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"someone@example.com","orgId":"org-123","orgName":"Example Org","configDirectory":"/home/someone/.claude","subscriptionType":"max"}"#;

/// Writes a script to `dir` that prints `output` and exits with `code`.
fn script(dir: &Path, name: &str, output: &str, code: i32) -> PathBuf {
    if cfg!(windows) {
        let path = dir.join(format!("{name}.cmd"));
        std::fs::write(
            &path,
            format!("@echo off\r\necho {output}\r\nexit /b {code}\r\n"),
        )
        .unwrap();
        path
    } else {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nprintf '%s\\n' '{output}'\nexit {code}\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }
}

/// [`keyed_config`] with a `claude-cli` entry for each name and command.
fn config(entries: &[(&str, &Path)]) -> Config {
    let mut config = keyed_config();
    config.claude_cli = entries
        .iter()
        .map(|(name, command)| ClaudeCli {
            name: (*name).to_owned(),
            command: command.display().to_string(),
            ..ClaudeCli::default()
        })
        .collect();
    config
}

/// Not upstream's: only whether the entry is signed in and how are passed
/// on, never the account, organization or directory Claude Code names,
/// whether it is signed in or not.
#[tokio::test]
async fn passes_on_only_whether_it_is_signed_in_and_how() {
    let dir = tempfile::tempdir().unwrap();
    let signed_in = script(dir.path(), "signed-in", SIGNED_IN, 0);
    let signed_out = script(
        dir.path(),
        "signed-out",
        r#"{"loggedIn":false,"authMethod":"none","apiProvider":"firstParty"}"#,
        1,
    );
    let dash = Dash::with_config(config(&[("max-1", &signed_in), ("max-2", &signed_out)]));

    let answer = dash.get(&format!("{STATUS}?name=max-1")).await;
    assert_eq!(
        answer.json(StatusCode::OK),
        json!({"loggedIn": true, "authMethod": "claude.ai"})
    );
    for private in [
        "someone",
        "example",
        "org-123",
        "Example Org",
        ".claude",
        "max\"",
    ] {
        assert!(!answer.body.contains(private), "{}", answer.body);
    }
    // Names match as the config's uniqueness rule does.
    let answer = dash.get(&format!("{STATUS}?name=%20MAX-2")).await;
    assert_eq!(
        answer.json(StatusCode::OK),
        json!({"loggedIn": false, "authMethod": "none"})
    );
}

/// Not upstream's: the route's refusals and Claude Code's failures.
#[tokio::test]
async fn refuses_and_reports_failures() {
    let dir = tempfile::tempdir().unwrap();
    let garbled = script(dir.path(), "garbled", "Not logged in", 1);
    let missing = dir.path().join("no-such-claude");
    let dash = Dash::with_config(config(&[("garbled", &garbled), ("missing", &missing)]));

    for path in [
        STATUS.to_owned(),
        format!("{STATUS}?name="),
        format!("{STATUS}?name=%20"),
    ] {
        let message = dash
            .get(&path)
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_request");
        assert!(message.contains("name"), "{message}");
    }
    let message = dash
        .get(&format!("{STATUS}?name=a&name=b"))
        .await
        .error(StatusCode::BAD_REQUEST, "invalid_request");
    assert!(message.contains("more than once"), "{message}");
    let message = dash
        .get(&format!("{STATUS}?name=other"))
        .await
        .error(StatusCode::NOT_FOUND, "not_found");
    assert!(message.contains("\"other\""), "{message}");

    let message = dash
        .get(&format!("{STATUS}?name=garbled"))
        .await
        .error(StatusCode::BAD_GATEWAY, "claude_cli_failed");
    assert!(message.contains("wasn't the JSON expected"), "{message}");
    let message = dash
        .get(&format!("{STATUS}?name=missing"))
        .await
        .error(StatusCode::BAD_GATEWAY, "claude_cli_failed");
    assert!(message.contains("couldn't run Claude Code"), "{message}");

    dash.call(Method::POST, &format!("{STATUS}?name=garbled"), "")
        .await
        .error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
}

/// Registers the credentials the server makes of `config`, as the service
/// does, except those `skip` names, each first changed by `change`.
fn register(dash: &Dash, config: &Config, skip: &[&str], change: impl Fn(&mut Auth)) {
    let ctx = SynthesisContext::new("", Utc::now());
    let auths = synthesize_config_auths(config, &ctx, &mut StableIdGenerator::new()).unwrap();
    for mut auth in auths {
        if !skip.contains(&auth.label.as_str()) {
            change(&mut auth);
            dash.manager().register_unsaved(auth).unwrap();
        }
    }
}

/// A `claude-cli` entry named `name`.
fn entry(name: &str) -> ClaudeCli {
    ClaudeCli {
        name: name.to_owned(),
        ..ClaudeCli::default()
    }
}

/// Not upstream's: each entry, in the config's order, with its name,
/// prefix and config directory as the config has them, and the credential
/// the server made of it with its state, cooldowns, quota and last error;
/// a disabled entry, or one the server has made no credential of yet, has
/// none. Neither the entry's command nor anything in its config directory
/// is in the answer.
#[tokio::test]
async fn entries_show_each_entry_with_its_credential() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("claude-home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(
        home.join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-in-the-file"}}"#,
    )
    .unwrap();
    let home = home.display().to_string();
    let mut config = keyed_config();
    // Another provider's entry, whose credential this route leaves out.
    config.claude_api_key = vec![ClaudeKey {
        api_key: "sk-ant-api-key-of-claude".into(),
        ..ClaudeKey::default()
    }];
    config.claude_cli = vec![
        ClaudeCli {
            name: " max-1 ".into(),
            prefix: " team ".into(),
            config_dir: format!(" {home} "),
            command: "/opt/private-place/claude".into(),
            ..ClaudeCli::default()
        },
        ClaudeCli {
            config_dir: "/srv/off".into(),
            disabled: true,
            ..entry("off")
        },
        entry("max-3"),
        entry("max-4"),
    ];
    let dash = Dash::with_config(config.clone());
    let now = Utc::now();
    register(&dash, &config, &["max-4"], |auth| {
        if auth.label != "max-1" {
            return;
        }
        auth.status = Status::Error;
        auth.status_message = "unauthorized".into();
        auth.unavailable = true;
        auth.next_retry_after = Some(now + Duration::minutes(10));
        auth.last_error = Some(AuthError {
            message: json!({
                "type": "error",
                "error": {"type": "authentication_error", "message": "Not signed in to Claude Code"},
            })
            .to_string(),
            http_status: 401,
            ..AuthError::default()
        });
        auth.quota.observed_at = Some(now - Duration::minutes(1));
        auth.quota.signals = [
            ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.4"),
            ("Anthropic-Ratelimit-Unified-7d-Status", "allowed"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
        auth.success = 3;
        auth.failed = 1;
    });

    let answer = dash.get(ENTRIES).await;
    let body = answer.json(StatusCode::OK);
    let entries = body["entries"].as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["max-1", "off", "max-3", "max-4"]);

    let first = &entries[0];
    assert_eq!(first["prefix"], "team");
    assert_eq!(first["config_dir"], home.as_str());
    assert_eq!(first["disabled"], false);
    assert_eq!(
        first["last_error"],
        json!({"message": "Not signed in to Claude Code", "http_status": 401})
    );
    let credential = &first["credential"];
    let id = credential["id"].as_str().unwrap();
    assert!(id.starts_with("claude-cli:"), "{id}");
    let auth = dash.manager().get(id).unwrap();
    assert_eq!(credential["auth_index"], auth.index.as_str());
    assert_eq!(credential["provider"], "claude-cli");
    assert_eq!(credential["label"], "max-1");
    assert_eq!(credential["status"], "error");
    assert_eq!(credential["status_message"], "unauthorized");
    assert_eq!(credential["unavailable"], true);
    assert_eq!(credential["success"], 3);
    assert_eq!(credential["failed"], 1);
    assert!(credential["next_retry_after"].is_string(), "{credential}");
    let cooldowns = credential["cooldowns"].as_array().unwrap();
    assert_eq!(cooldowns.len(), 1, "{credential}");
    assert_eq!(cooldowns[0]["scope"], "credential");
    assert_eq!(cooldowns[0]["reason"], "unauthorized");
    assert!(cooldowns[0]["remaining_seconds"].as_i64().unwrap() > 500);
    assert_eq!(
        credential["quota"]["signals"],
        json!({
            "Anthropic-Ratelimit-Unified-5h-Utilization": "0.4",
            "Anthropic-Ratelimit-Unified-7d-Status": "allowed",
        })
    );
    assert!(
        credential["quota"]["observed_at"].is_string(),
        "{credential}"
    );

    let off = &entries[1];
    assert_eq!(off["disabled"], true);
    assert_eq!(off["config_dir"], "/srv/off");
    assert_eq!(off["credential"], Value::Null);
    assert_eq!(off["last_error"], Value::Null);

    let third = &entries[2];
    assert_eq!(third["prefix"], "");
    assert_eq!(third["config_dir"], "");
    assert_eq!(third["credential"]["label"], "max-3");
    assert_eq!(third["credential"]["status"], "active");
    assert_eq!(third["credential"]["cooldowns"], json!([]));
    assert_eq!(third["last_error"], Value::Null);
    assert_ne!(third["credential"]["id"], credential["id"]);

    assert_eq!(entries[3]["credential"], Value::Null);

    for private in [
        "private-place",
        "\"command\"",
        "sk-ant-oat-in-the-file",
        "claudeAiOauth",
        "sk-ant-api-key-of-claude",
    ] {
        assert!(!answer.body.contains(private), "{}", answer.body);
    }
}

/// Not upstream's: a last error that isn't in the form of Anthropic's
/// error body is passed on as it is, without a status when it had none;
/// no entries is an empty list; the route needs the key.
#[tokio::test]
async fn entries_pass_other_errors_on_and_need_the_key() {
    let mut config = keyed_config();
    config.claude_cli = vec![entry("plain")];
    let dash = Dash::with_config(config.clone());
    let message = "claude-cli plain: Claude Code ended without an answer";
    register(&dash, &config, &[], |auth| {
        auth.last_error = Some(AuthError {
            message: message.into(),
            ..AuthError::default()
        });
    });
    let body = dash.get(ENTRIES).await.json(StatusCode::OK);
    assert_eq!(
        body["entries"][0]["last_error"],
        json!({"message": message, "http_status": null})
    );

    let body = Dash::new().get(ENTRIES).await.json(StatusCode::OK);
    assert_eq!(body, json!({"entries": []}));

    dash.send(request(LOCAL, Method::GET, ENTRIES, ""))
        .await
        .error(StatusCode::UNAUTHORIZED, "missing_management_key");
}
