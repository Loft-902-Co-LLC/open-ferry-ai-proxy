// Ported from CLIProxyAPI internal/cmd/openai_login.go, openai_device_login.go
// and anthropic_login.go; the prompts in sdk/auth/codex.go, codex_device.go
// and claude.go; the saving in sdk/auth/manager.go's Login;
// internal/util/ssh_helper.go's PrintSSHTunnelInstructions; and
// GetUserFriendlyMessage in internal/auth/codex/errors.go, which
// internal/auth/claude repeats (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `-codex-login`, `-codex-device-login` and `-claude-login` commands.
//!
//! A login prints its URL or device code, waits for the user, and saves the
//! credential in the auth directory: over the file a past login for the
//! account saved, keeping that file's settings, and in place of a Claude
//! file saved under an older name.
//!
//! Deviations from upstream:
//! - The SSH tunnel hint names `<server-address>`; upstream asks outside
//!   services for the machine's public IP.
//! - The URL is printed even when the browser opens, in case it opened
//!   nowhere useful. Upstream first checks a browser is there and prints the
//!   URL only when it isn't or fails to open.
//! - The port-in-use message names the port; upstream's always says 3000.
//! - A failed login exits with 1. Upstream exits with 0, except with 13 for a
//!   port in use, as here.

use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

use open_ferry_core::auth::metadata::merge_existing_auth_metadata;
use open_ferry_core::auth::{Auth, AuthStore, FileStore};
use open_ferry_core::config::Config;
use open_ferry_providers::claude::oauth::{self as claude_oauth, ClaudeAuth};
use open_ferry_providers::claude::token::find_matching_legacy_credential;
use open_ferry_providers::codex::oauth::{self as codex_oauth, CodexAuth};

use crate::browser;

/// Which login to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Login {
    /// `-codex-login`.
    Codex,
    /// `-codex-device-login`.
    CodexDevice,
    /// `-claude-login`.
    Claude,
}

impl Login {
    /// The name in the outcome's message.
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::CodexDevice => "Codex device",
            Self::Claude => "Claude",
        }
    }

    /// The provider's callback port.
    fn default_port(self) -> u16 {
        match self {
            Self::Codex | Self::CodexDevice => codex_oauth::DEFAULT_CALLBACK_PORT,
            Self::Claude => claude_oauth::DEFAULT_CALLBACK_PORT,
        }
    }
}

/// How a login runs.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Print the URL rather than open a browser.
    pub no_browser: bool,
    /// The callback port, or 0 or less for the provider's.
    pub callback_port: i64,
}

/// The exit code for a port in use (upstream's `ErrPortInUse.Code`).
const PORT_IN_USE_EXIT: u8 = 13;

/// Why a login failed, as the command reports it.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// Upstream's `AuthenticationError`, by its type.
    Authentication(&'static str),
    /// Anything else, by its message.
    Other(String),
}

/// Runs `login` and saves its credential in `auth_dir`.
pub async fn run(login: Login, config: &Config, auth_dir: &Path, options: Options) -> ExitCode {
    let port = match callback_port(options.callback_port, login.default_port()) {
        Some(port) => port,
        // Upstream can't listen on such a port, and takes it as in use.
        None => return report(login, Failure::Authentication("port_in_use"), options),
    };
    let result = match login {
        Login::Codex => {
            let auth = CodexAuth::with_proxy_url(&config.proxy_url);
            let login_options = codex_oauth::LoginOptions {
                callback_port: port,
                ..codex_oauth::LoginOptions::default()
            };
            codex_oauth::login(&auth, login_options, |prompt| {
                present_url(
                    "Codex",
                    &prompt.url,
                    prompt.callback_port,
                    options.no_browser,
                );
            })
            .await
            .map_err(codex_failure)
        }
        Login::CodexDevice => {
            let auth = CodexAuth::with_proxy_url(&config.proxy_url);
            codex_oauth::login_with_device_code(&auth, |code| {
                print!("{}", device_prompt(&code.verification_url, &code.user_code));
                if !options.no_browser
                    && let Err(error) = browser::open(&code.verification_url)
                {
                    tracing::warn!("Failed to open browser automatically: {error}");
                }
            })
            .await
            .map_err(codex_failure)
        }
        Login::Claude => {
            let auth = ClaudeAuth::with_proxy_url(&config.proxy_url);
            let login_options = claude_oauth::LoginOptions {
                callback_port: port,
                ..claude_oauth::LoginOptions::default()
            };
            claude_oauth::login(&auth, login_options, |prompt| {
                present_url(
                    "Claude",
                    &prompt.url,
                    prompt.callback_port,
                    options.no_browser,
                );
            })
            .await
            .map_err(claude_failure)
        }
    };
    let mut auth = match result {
        Ok(auth) => auth,
        Err(failure) => return report(login, failure, options),
    };
    println!(
        "{} authentication successful",
        if login == Login::Claude {
            "Claude"
        } else {
            "Codex"
        }
    );
    match save(&mut auth, auth_dir) {
        Ok(path) => {
            if !path.is_empty() {
                println!("Authentication saved to {path}");
            }
            println!("{} authentication successful!", login.name());
            ExitCode::SUCCESS
        }
        Err(message) => report(login, Failure::Other(message), options),
    }
}

/// The port to listen on: the provider's for 0 or less, and `None` for one
/// out of range.
fn callback_port(flag: i64, default: u16) -> Option<u16> {
    if flag <= 0 {
        return Some(default);
    }
    u16::try_from(flag).ok()
}

fn codex_failure(error: codex_oauth::LoginError) -> Failure {
    match error {
        codex_oauth::LoginError::Authentication(error) => {
            Failure::Authentication(error.kind.as_str())
        }
        error => Failure::Other(error.to_string()),
    }
}

fn claude_failure(error: claude_oauth::LoginError) -> Failure {
    match error {
        claude_oauth::LoginError::Authentication(error) => {
            Failure::Authentication(error.kind.as_str())
        }
        error => Failure::Other(error.to_string()),
    }
}

/// Reports a failure and gives the exit code.
fn report(login: Login, failure: Failure, options: Options) -> ExitCode {
    match failure {
        Failure::Authentication(kind) => {
            let port = callback_port(options.callback_port, login.default_port());
            tracing::error!("{}", friendly_message(kind, port));
            if kind == "port_in_use" {
                ExitCode::from(PORT_IN_USE_EXIT)
            } else {
                ExitCode::FAILURE
            }
        }
        Failure::Other(message) => {
            println!("{} authentication failed: {message}", login.name());
            ExitCode::FAILURE
        }
    }
}

/// Upstream's `GetUserFriendlyMessage` for an `AuthenticationError` of type
/// `kind`, naming `port` when it is the one in use.
fn friendly_message(kind: &str, port: Option<u16>) -> String {
    match kind {
        "token_expired" => "Your authentication has expired. Please log in again.".into(),
        "token_invalid" => "Your authentication is invalid. Please log in again.".into(),
        "authentication_required" => "Please log in to continue.".into(),
        "port_in_use" => {
            let port = port.map_or_else(|| "the callback port".to_owned(), |p| format!("port {p}"));
            format!(
                "The required port is already in use. Please close any applications using {port} and try again."
            )
        }
        "callback_timeout" => "Authentication timed out. Please try again.".into(),
        "browser_open_failed" => {
            "Could not open your browser automatically. Please copy and paste the URL manually."
                .into()
        }
        _ => "Authentication failed. Please try again.".into(),
    }
}

/// Shows a browser login's URL, opening it unless `no_browser`.
fn present_url(provider: &str, url: &str, port: u16, no_browser: bool) {
    if !no_browser {
        println!("Opening browser for {provider} authentication");
        if let Err(error) = browser::open(url) {
            tracing::warn!("Failed to open browser automatically: {error}");
        }
    }
    print!("{}", ssh_tunnel_instructions(port));
    println!("Visit the following URL to continue authentication:\n{url}");
    println!("Waiting for {provider} authentication callback...");
}

/// What the device login prints.
fn device_prompt(verification_url: &str, user_code: &str) -> String {
    format!(
        "Starting Codex device authentication...\nCodex device URL: {verification_url}\nCodex device code: {user_code}\n"
    )
}

/// How to reach the callback server from another machine.
fn ssh_tunnel_instructions(port: u16) -> String {
    let border = "=".repeat(80);
    let mut out = String::new();
    let _ = write!(
        out,
        "To authenticate from a remote machine, an SSH tunnel may be required.\n\
         {border}\n\
         \x20 Run one of the following commands on your local machine (NOT the server):\n\
         \n\
         \x20 # Standard SSH command (assumes SSH port 22):\n\
         \x20 ssh -L {port}:127.0.0.1:{port} root@<server-address> -p 22\n\
         \n\
         \x20 # If using an SSH key (assumes SSH port 22):\n\
         \x20 ssh -i <path_to_your_key> -L {port}:127.0.0.1:{port} root@<server-address> -p 22\n\
         \n\
         \x20 NOTE: If your server's SSH port is not 22, please modify the '-p 22' part accordingly.\n\
         {border}\n"
    );
    out
}

/// Saves a login's credential as upstream's `Manager.Login` does, and
/// returns its path.
fn save(auth: &mut Auth, auth_dir: &Path) -> Result<String, String> {
    let store = FileStore::new(auth_dir);
    store.merge_existing(auth);
    let legacy =
        find_matching_legacy_credential(&store, auth).map_err(|error| error.to_string())?;
    if let Some(legacy) = &legacy {
        merge_existing_auth_metadata(auth, &legacy.metadata);
    }
    let path = store
        .save_new_auth(auth)
        .map_err(|error| error.to_string())?;
    if let Some(legacy) = legacy {
        if path.trim().is_empty() {
            return Err(
                "canonical Claude credential was not persisted; legacy credential retained".into(),
            );
        }
        let id = match legacy.id.trim() {
            "" => legacy.file_name.trim(),
            id => id,
        };
        store.delete(id).map_err(|error| {
            format!(
                "canonical Claude credential saved but legacy credential cleanup failed: {error}"
            )
        })?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn callback_port_falls_back_to_the_providers() {
        assert_eq!(callback_port(0, 1455), Some(1455));
        assert_eq!(callback_port(-1, 1455), Some(1455));
        assert_eq!(callback_port(8080, 1455), Some(8080));
        assert_eq!(callback_port(70_000, 1455), None);
    }

    #[test]
    fn messages_are_upstreams() {
        assert_eq!(
            friendly_message("callback_timeout", None),
            "Authentication timed out. Please try again."
        );
        assert_eq!(
            friendly_message("code_exchange_failed", None),
            "Authentication failed. Please try again."
        );
        assert_eq!(
            friendly_message("port_in_use", Some(1455)),
            "The required port is already in use. Please close any applications using port 1455 and try again."
        );
        assert!(friendly_message("port_in_use", None).contains("using the callback port"));
        assert_eq!(
            device_prompt("https://auth.example/device", "ABCD-EFGH"),
            "Starting Codex device authentication...\nCodex device URL: https://auth.example/device\nCodex device code: ABCD-EFGH\n"
        );
        let hint = ssh_tunnel_instructions(54545);
        let lines: Vec<&str> = hint.lines().collect();
        assert_eq!(lines.len(), 12);
        assert_eq!(lines[1], "=".repeat(80));
        assert_eq!(
            lines[5],
            "  ssh -L 54545:127.0.0.1:54545 root@<server-address> -p 22"
        );
        assert_eq!(lines[11], lines[1]);
    }

    #[test]
    fn port_in_use_exits_with_13() {
        let options = Options::default();
        let code = report(
            Login::Claude,
            Failure::Authentication("port_in_use"),
            options,
        );
        assert_eq!(code, ExitCode::from(13));
        let code = report(Login::Codex, Failure::Other("x".into()), options);
        assert_eq!(code, ExitCode::FAILURE);
    }

    fn claude_auth(file_name: &str, metadata: Value) -> Auth {
        let Value::Object(metadata) = metadata else {
            unreachable!()
        };
        Auth {
            id: file_name.into(),
            file_name: file_name.into(),
            provider: "claude".into(),
            metadata,
            ..Auth::default()
        }
    }

    #[test]
    fn saving_replaces_a_legacy_claude_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let mut legacy = claude_auth(
            "claude-a@b.c.json",
            json!({"type": "claude", "email": "a@b.c", "account_uuid": "acct",
                   "access_token": "old", "prefix": "team"}),
        );
        store.save_new_auth(&mut legacy).unwrap();

        let name = open_ferry_providers::claude::token::credential_file_name("a@b.c", "", "acct");
        let mut auth = claude_auth(
            &name,
            json!({"type": "claude", "email": "a@b.c", "account_uuid": "acct",
                   "access_token": "new"}),
        );
        let path = save(&mut auth, dir.path()).unwrap();
        assert_eq!(Path::new(&path), dir.path().join(&name));
        assert!(!dir.path().join("claude-a@b.c.json").exists());
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["access_token"], "new");
        assert_eq!(saved["prefix"], "team");
    }

    #[test]
    fn saving_keeps_the_settings_of_the_file_it_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let mut old = Auth {
            id: "codex-a@b.c-plus.json".into(),
            file_name: "codex-a@b.c-plus.json".into(),
            provider: "codex".into(),
            ..Auth::default()
        };
        old.metadata.insert("type".into(), json!("codex"));
        old.metadata.insert("access_token".into(), json!("old"));
        old.metadata.insert("disabled".into(), json!(true));
        old.disabled = true;
        store.save_new_auth(&mut old).unwrap();

        let mut auth = Auth {
            id: "codex-a@b.c-plus.json".into(),
            file_name: "codex-a@b.c-plus.json".into(),
            provider: "codex".into(),
            ..Auth::default()
        };
        auth.metadata.insert("type".into(), json!("codex"));
        auth.metadata.insert("access_token".into(), json!("new"));
        let path = save(&mut auth, dir.path()).unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["access_token"], "new");
        assert_eq!(saved["disabled"], true);
        assert!(auth.disabled);
    }
}
