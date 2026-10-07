//! Asking an entry's Claude Code whether it is signed in.
//!
//! `claude auth status` tells more than that: the account's email and
//! organization, and where its config lives. Only whether it is signed in
//! and how are kept; the rest is neither returned nor logged.

use std::fmt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::process::Command;

use super::process::no_window;
use super::settings::Entry;

/// How long `auth status` may take.
const STATUS_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether an entry's Claude Code is signed in, and how.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AuthStatus {
    /// Whether it is signed in.
    #[serde(rename = "loggedIn")]
    pub logged_in: bool,
    /// How: a subscription sign-in, a token, a key; empty when it doesn't
    /// say.
    #[serde(rename = "authMethod")]
    pub auth_method: String,
}

/// Why [`auth_status`] has no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusError {
    /// Claude Code couldn't be run.
    Run(String),
    /// It didn't answer in time.
    Timeout,
    /// Its answer wasn't the JSON expected.
    Output,
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Run(error) => write!(f, "couldn't run Claude Code: {error}"),
            Self::Timeout => write!(
                f,
                "Claude Code didn't answer within {}s",
                STATUS_TIMEOUT.as_secs()
            ),
            Self::Output => write!(f, "Claude Code's answer wasn't the JSON expected"),
        }
    }
}

impl std::error::Error for StatusError {}

/// Runs `<command> auth status --json` for `entry`, in its environment and
/// working directory under `work_root`, and gives back only `loggedIn` and
/// `authMethod`. Claude Code exits with an error when it isn't signed in;
/// its answer counts all the same.
pub async fn auth_status(entry: &Entry, work_root: &Path) -> Result<AuthStatus, StatusError> {
    let program = entry.program().map_err(StatusError::Run)?;
    let (_, work) = entry.dirs(work_root).map_err(|error| {
        StatusError::Run(format!("couldn't make the entry's directory: {error}"))
    })?;
    let mut command = Command::new(&program);
    command
        .args(["auth", "status", "--json"])
        .current_dir(work)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    entry.apply_env(&mut command);
    no_window(&mut command);
    let output = match tokio::time::timeout(STATUS_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Err(StatusError::Run(format!("{}: {error}", program.display())));
        }
        Err(_) => return Err(StatusError::Timeout),
    };
    parse(&output.stdout)
}

/// `loggedIn` and `authMethod` from Claude Code's answer, and nothing else.
fn parse(stdout: &[u8]) -> Result<AuthStatus, StatusError> {
    let value: Value = serde_json::from_slice(stdout).map_err(|_| StatusError::Output)?;
    let logged_in = value
        .get("loggedIn")
        .and_then(Value::as_bool)
        .ok_or(StatusError::Output)?;
    let auth_method = value
        .get("authMethod")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(AuthStatus {
        logged_in,
        auth_method,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_two_fields() {
        let status = parse(
            br#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"me@example.com","orgId":"o","orgName":"Org","configDirectory":"/home/me/.claude","subscriptionType":"max"}"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            serde_json::json!({"loggedIn": true, "authMethod": "claude.ai"})
        );
        assert_eq!(
            parse(br#"{"loggedIn":false}"#).unwrap(),
            AuthStatus {
                logged_in: false,
                auth_method: String::new()
            }
        );
        assert_eq!(parse(b"Not logged in"), Err(StatusError::Output));
        assert_eq!(parse(br#"{"authMethod":"x"}"#), Err(StatusError::Output));
    }
}
