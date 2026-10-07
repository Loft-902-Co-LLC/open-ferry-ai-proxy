//! `GET /claude-cli/auth-status`, with a script standing in for Claude
//! Code, which prints what `claude auth status --json` prints.

use std::path::{Path, PathBuf};

use http::{Method, StatusCode};
use open_ferry_core::config::{ClaudeCli, Config};
use serde_json::json;

use super::{Dash, keyed_config};

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
