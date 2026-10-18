// Ported from CLIProxyAPI internal/cmd/siwc_bind.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Explicit SIWC binding, without replacing outer Codex tokens or settings.
//! The ID token's signature and nonce aren't verified, and the SIWC account
//! isn't matched to the outer account. Persistence is validated under the
//! credential lock and off the runtime worker.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use open_ferry_core::auth::codex_refresh::{generate_device_id, validate_device_id};
use open_ferry_core::auth::credential_lock::acquire_credential_lock;
use open_ferry_core::auth::file_guard::FileGuard;
use open_ferry_core::auth::file_store::BindIdentity;
use open_ferry_core::auth::{Auth, FileStore};
use open_ferry_core::config::Config;
use open_ferry_providers::codex::siwc::{self, SiwcAuth};
use open_ferry_providers::oauth::page::{Failure, Outcome};
use open_ferry_providers::oauth::{
    CallbackServer, OAuthCallback, Pkce, generate_state, parse_oauth_callback,
};
use serde_json::{Map, Value};

use crate::login::{Options, present_url};

const WAIT: Duration = Duration::from_secs(300);
const PASTE_AFTER: Duration = Duration::from_secs(15);

struct Target {
    path: PathBuf,
    directory: PathBuf,
    auth_directory: PathBuf,
    store: FileStore,
    identity: BindIdentity,
    record: Auth,
    device_id: String,
}

fn resolve(auth_dir: &Path, guard: Option<FileGuard>, raw: &str) -> io::Result<Target> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(io::Error::other(
            "siwc-bind: missing codex credential file path",
        ));
    }
    if auth_dir.as_os_str().is_empty() {
        return Err(io::Error::other("siwc-bind: auth-dir is not configured"));
    }
    let directory = auth_dir.canonicalize()?;
    let candidate = Path::new(raw);
    let candidate = if candidate.is_absolute() {
        candidate.to_owned()
    } else {
        if raw.contains(['/', '\\']) || matches!(raw, "." | "..") {
            return Err(io::Error::other(
                "siwc-bind: expected an absolute path or bare filename",
            ));
        }
        auth_dir.join(candidate)
    };
    let path = candidate;
    if !path
        .parent()
        .ok_or_else(|| io::Error::other("siwc-bind: missing target parent"))?
        .canonicalize()?
        .starts_with(&directory)
    {
        return Err(io::Error::other(
            "siwc-bind: credential file must belong to auth-dir",
        ));
    }
    let store = FileStore::new(auth_dir).with_file_guard(guard);
    let (metadata, file_id) = store.read_bind_target(&path)?;
    require_exact_leaf(&path)?;
    let identity = BindIdentity::capture(&metadata, file_id);
    let device_id = metadata
        .get("siwc")
        .and_then(|v| v.get("device_id"))
        .and_then(Value::as_str)
        .and_then(validate_device_id)
        .unwrap_or_else(generate_device_id);
    let mut record = Auth {
        provider: "codex".into(),
        file_name: path.to_string_lossy().into_owned(),
        metadata,
        ..Auth::default()
    };
    record
        .attributes
        .insert("path".into(), path.to_string_lossy().into_owned());
    Ok(Target {
        path,
        directory,
        auth_directory: auth_dir.to_owned(),
        store,
        identity,
        record,
        device_id,
    })
}

async fn save(target: Target, tokens: Map<String, Value>) -> io::Result<()> {
    let guard = acquire_credential_lock(&target.path).await?;
    // Keep the unresolved leaf and upstream lock spelling. Recheck only
    // the parent and exact leaf spelling; strict persistence refuses links and aliases.
    let work = move || {
        if !target
            .path
            .parent()
            .ok_or_else(|| io::Error::other("siwc-bind: missing target parent"))?
            .canonicalize()?
            .starts_with(&target.directory)
        {
            return Err(io::Error::other(
                "siwc-bind: credential target changed during sign-in",
            ));
        }
        require_exact_leaf(&target.path)?;
        target
            .store
            .bind_siwc_locked(&target.record, tokens, &guard, &|disk, id| {
                if target.auth_directory.canonicalize()? != target.directory {
                    return Err(io::Error::other("siwc-bind: auth directory changed"));
                }
                target.identity.check(disk, id)
            })?;
        Ok(())
    };
    #[allow(clippy::disallowed_methods)]
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| io::Error::other("siwc-bind: persistence task failed"))?
}

fn require_exact_leaf(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("siwc-bind: missing target parent"))?
        .canonicalize()?;
    let leaf = path
        .file_name()
        .ok_or_else(|| io::Error::other("siwc-bind: missing target parent"))?;
    #[cfg(windows)]
    if leaf.to_string_lossy().contains(':') {
        return Err(io::Error::other(
            "siwc-bind: credential leaf has an alternate spelling",
        ));
    }
    if path.canonicalize()? != parent.join(leaf) {
        return Err(io::Error::other(
            "siwc-bind: credential leaf has an alternate spelling",
        ));
    }
    Ok(())
}

// Only these fixed literals may reach stderr or the callback page.
fn safe_error(error: &io::Error) -> &'static str {
    match error.to_string().as_str() {
        "SIWC bind target is a leaf link" => "credential target is a leaf link",
        "SIWC bind alias scan failed" => "credential alias scan failed",
        "SIWC bind target changed before replacement" => {
            "credential target changed before replacement"
        }
        "siwc-bind: credential leaf has an alternate spelling" => {
            "credential leaf has an alternate spelling"
        }
        "SIWC bind target has a linked alias" => "credential target has a linked alias",
        "SIWC bind target identity changed" => "credential identity changed during sign-in",
        "invalid SIWC bind target" => "credential target is not a regular file",
        "siwc-bind: missing target parent" => "credential target has no parent",
        "invalid SIWC bind target JSON" => "credential JSON is invalid",
        "SIWC bind requires a Codex credential" => "credential is not Codex",
        "SIWC bind requires outer tokens" => "credential is missing outer tokens",
        "invalid SIWC bind settings" => "credential settings are invalid",
        "SIWC bind target exceeds size cap" => "credential exceeds size cap",
        "siwc-bind: missing codex credential file path" => "credential target is missing",
        "siwc-bind: auth-dir is not configured" => "auth directory is not configured",
        "siwc-bind: expected an absolute path or bare filename" => {
            "expected an absolute path or bare filename"
        }
        "siwc-bind: credential file must belong to auth-dir" => {
            "credential is outside auth directory"
        }
        "siwc-bind: credential target changed during sign-in"
        | "siwc-bind: auth directory changed" => "auth directory changed during sign-in",
        "siwc-bind: persistence task failed" => "credential persistence task failed",
        _ => match error.kind() {
            io::ErrorKind::NotFound => "credential or auth directory is missing",
            io::ErrorKind::PermissionDenied => "credential access was denied",
            io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => {
                "credential target is invalid"
            }
            _ => "credential I/O failed",
        },
    }
}

async fn read_paste() -> Result<OAuthCallback, &'static str> {
    tokio::time::sleep(PASTE_AFTER).await;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // A detached stdin reader must not occupy Tokio's blocking pool: a
    // browser callback can leave it waiting for Enter when the process exits.
    std::thread::spawn(move || {
        print!("Paste the callback URL (or press Enter to keep waiting): ");
        let _ = io::stdout().flush();
        let mut input = String::new();
        let value = io::stdin().read_line(&mut input).map(|_| input);
        let _ = sender.send(value);
    });
    match receiver.await {
        Ok(Ok(input)) => match parse_oauth_callback(&input) {
            Ok(Some(callback)) => Ok(callback),
            Ok(None) => std::future::pending().await,
            Err(_) => Err("invalid pasted callback"),
        },
        _ => Err("failed to read pasted callback"),
    }
}

async fn flow(
    target: Target,
    auth: SiwcAuth,
    port: u16,
    wait: Duration,
    present: impl FnOnce(&str, u16),
    paste: impl std::future::Future<Output = Result<OAuthCallback, &'static str>>,
) -> Result<(), (u8, &'static str)> {
    let state = generate_state();
    let nonce = generate_state();
    let pkce = Pkce::generate();
    let mut server =
        match CallbackServer::start(port, "/auth/callback", &state, "Sign in with ChatGPT").await {
            Ok(server) => server,
            Err(error) => {
                return Err(if error.kind() == io::ErrorKind::AddrInUse {
                    (13, "the callback port is already in use")
                } else {
                    (1, "failed to start callback server")
                });
            }
        };
    let redirect = siwc::redirect_uri(server.port());
    let url = match auth.generate_auth_url(&state, &nonce, &target.device_id, &redirect, &pkce) {
        Ok(url) => url,
        Err(_) => {
            server
                .finish(Outcome::Failed(Failure::new(
                    "Binding failed",
                    "failed to generate authorization URL",
                )))
                .await;
            return Err((1, "failed to generate authorization URL"));
        }
    };
    present(&url, server.port());
    let callback = tokio::select! {
        result = server.wait_for_callback(wait) => result.map_err(|_| "callback wait failed or timed out"),
        result = paste => result,
    };
    let result = match callback {
        Ok(callback) => exchange_and_save(&auth, target, callback, &state, &redirect, &pkce).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => {
            server.finish(Outcome::SignedIn).await;
            Ok(())
        }
        Err(reason) => {
            server
                .finish(Outcome::Failed(Failure::new("Binding failed", reason)))
                .await;
            Err((1, reason))
        }
    }
}

async fn exchange_and_save(
    auth: &SiwcAuth,
    target: Target,
    callback: OAuthCallback,
    state: &str,
    redirect: &str,
    pkce: &Pkce,
) -> Result<(), &'static str> {
    if callback.state != state {
        return Err("state mismatch");
    }
    if !callback.error.is_empty() {
        return Err("OAuth callback reported an error");
    }
    if callback.client_id.trim().is_empty() {
        return Err("callback did not contain an issued client ID");
    }
    let tokens = tokio::time::timeout(
        Duration::from_secs(60),
        auth.exchange_code_for_tokens(
            &callback.code,
            &callback.client_id,
            redirect,
            &target.device_id,
            pkce,
        ),
    )
    .await
    .map_err(|_| "token exchange timed out")?
    .map_err(|_| "token exchange failed")?;
    save(target, tokens.to_json())
        .await
        .map_err(|error| safe_error(&error))
}

pub(crate) async fn run(
    config: &Config,
    auth_dir: &Path,
    name: &str,
    options: Options,
) -> ExitCode {
    // Not upstream's: as for a login, a privileged bind works only in a
    // trusted auth directory, and its store reads and replaces only trusted
    // credential files.
    let probe = crate::scope::runtime_probe();
    let directory = crate::scope::absolute_auth_dir(probe.as_ref(), auth_dir.to_path_buf());
    match crate::scope::authorize_dir(probe.as_ref(), &directory) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("siwc-bind: credential or auth directory is missing");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            tracing::error!("{error}");
            return ExitCode::FAILURE;
        }
    }
    let guard = crate::scope::credential_file_guard(probe);
    let name = name.to_owned();
    #[allow(clippy::disallowed_methods)]
    let selected = tokio::task::spawn_blocking(move || resolve(&directory, guard, &name)).await;
    let target = match selected {
        Ok(Ok(target)) => target,
        error => {
            let reason = match error {
                Ok(Err(error)) => safe_error(&error),
                _ => "credential selection task failed",
            };
            eprintln!("siwc-bind: {reason}");
            return ExitCode::FAILURE;
        }
    };
    let port = if options.callback_port <= 0 {
        siwc::DEFAULT_CALLBACK_PORT
    } else {
        match u16::try_from(options.callback_port) {
            Ok(port) => port,
            Err(_) => {
                eprintln!("siwc-bind: callback port is outside the valid range");
                return ExitCode::from(13);
            }
        }
    };
    let code = flow(
        target,
        SiwcAuth::with_proxy_url(&config.proxy_url),
        port,
        WAIT,
        |url, port| present_url("Sign in with ChatGPT", url, port, options.no_browser),
        read_paste(),
    )
    .await;
    match code {
        Ok(()) => {
            println!("SIWC credentials bound and saved successfully!");
            ExitCode::SUCCESS
        }
        Err((code, reason)) => {
            eprintln!("siwc-bind: {reason}");
            ExitCode::from(code)
        }
    }
}

#[cfg(test)]
mod tests;
