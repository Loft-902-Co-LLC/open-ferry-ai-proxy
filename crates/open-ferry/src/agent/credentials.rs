//! `open-ferry credentials`: the running server's credentials (sign-ins,
//! credential files and `claude-cli` entries), each with its state;
//! turning one off and on, resetting its quota, removing its file, and
//! signing in to a provider through the management API.
//!
//! Every one of these needs the running server, as only it knows each
//! credential's state. A credential's name, label and account are shown
//! with their email addresses masked; no token is ever shown.
//!
//! A sign-in goes through the management API's login routes: the command
//! prints the address to open in a browser and waits for the sign-in to
//! finish. Codex is signed in to this way. The Claude sign-in is against
//! Anthropic's terms (see `docs/claude-subscription.md`), so it needs
//! `--yes`; a `claude-cli` entry signs in with Claude Code itself, which
//! this never runs.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::http::Method;
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::api::{Body, encode};
use super::guard::confirm;
use super::mask::mask_plain;
use super::target::{Server, probe};
use super::{Caller, Context, Failure, Outcome, Report};

/// The states a credential is in, as listed.
pub(crate) const STATES: [&str; 7] = [
    "ready",
    "resting",
    "failing",
    "refreshing",
    "waiting",
    "off",
    "unknown",
];

/// How often a waiting sign-in is asked about, on the command line.
const CLI_POLL: Duration = Duration::from_secs(2);

/// How long the command line waits for a sign-in: as long as the server
/// keeps it.
const CLI_WAIT: Duration = Duration::from_secs(5 * 60);

/// How often, and how long, a tool call waits for a sign-in it was given
/// the state of.
const MCP_POLL: Duration = Duration::from_secs(1);
const MCP_WAIT: Duration = Duration::from_secs(50);

/// `credentials list`'s input.
#[derive(Clone, Debug, Default)]
pub(crate) struct ListInput {
    /// Only those in this state.
    pub(crate) state: Option<String>,
    /// Only those of this provider.
    pub(crate) provider: Option<String>,
}

/// The input of a command on one credential.
#[derive(Clone, Debug)]
pub(crate) struct TargetInput {
    /// Its `auth_index`, ID or name, as listed.
    pub(crate) credential: String,
}

/// `credentials login`'s input.
#[derive(Clone, Debug)]
pub(crate) struct LoginInput {
    /// `codex` or `claude`.
    pub(crate) provider: String,
    /// The sign-in to wait for, from an earlier call.
    pub(crate) state: Option<String>,
    /// Wait for it to finish.
    pub(crate) wait: bool,
}

/// A credential, as listed.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Credential {
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) auth_index: Option<String>,
    pub(crate) provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
    /// `ready`, `resting`, `failing`, `refreshing`, `waiting`, `off` or
    /// `unknown`.
    pub(crate) state: &'static str,
    /// The server's own status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status_message: Option<String>,
    pub(crate) disabled: bool,
    /// `file`, `memory` or `claude-cli`.
    pub(crate) source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) priority: Option<Value>,
    /// The cooldown of the whole credential, if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cooldown: Option<Value>,
    /// How many of its models are cooling down.
    pub(crate) model_cooldowns: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) quota: Option<Value>,
    /// Its ID and file name, to find it by.
    #[serde(skip)]
    id: String,
    #[serde(skip)]
    raw_name: String,
}

fn text_field(entry: &Value, name: &str) -> Option<String> {
    entry
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// The state of the credential `entry` describes.
fn state_of(entry: &Value) -> &'static str {
    let status = entry
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let flag = |name: &str| entry.get(name).and_then(Value::as_bool).unwrap_or(false);
    let resting = entry
        .get("cooldowns")
        .and_then(Value::as_array)
        .is_some_and(|cooldowns| {
            cooldowns
                .iter()
                .any(|cooldown| cooldown.get("scope").and_then(Value::as_str) == Some("credential"))
        });
    if flag("disabled") || status == "disabled" {
        "off"
    } else if status == "error" {
        "failing"
    } else if status == "refreshing" {
        "refreshing"
    } else if status == "pending" {
        "waiting"
    } else if resting || flag("unavailable") {
        "resting"
    } else if status == "active" {
        "ready"
    } else {
        "unknown"
    }
}

/// The credential an `auth-files` entry describes.
fn credential(entry: &Value) -> Credential {
    let cooldowns = entry
        .get("cooldowns")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let scope = |cooldown: &Value, scope: &str| {
        cooldown.get("scope").and_then(Value::as_str) == Some(scope)
    };
    let cooldown = cooldowns
        .iter()
        .find(|cooldown| scope(cooldown, "credential"))
        .map(|cooldown| {
            let mut out = Map::new();
            for field in ["reason", "retry_at", "remaining_seconds"] {
                if let Some(value) = cooldown.get(field) {
                    out.insert(field.to_owned(), value.clone());
                }
            }
            Value::Object(out)
        });
    let raw_name = text_field(entry, "name").unwrap_or_default();
    Credential {
        name: mask_plain(&raw_name),
        auth_index: text_field(entry, "auth_index"),
        provider: text_field(entry, "provider")
            .or_else(|| text_field(entry, "type"))
            .unwrap_or_default(),
        label: text_field(entry, "label").map(|label| mask_plain(&label)),
        state: state_of(entry),
        status: text_field(entry, "status"),
        status_message: text_field(entry, "status_message").map(|message| mask_plain(&message)),
        disabled: entry
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        source: text_field(entry, "source").unwrap_or_else(|| "memory".to_owned()),
        account: text_field(entry, "email")
            .or_else(|| text_field(entry, "account"))
            .map(|account| mask_plain(&account)),
        priority: entry
            .get("priority")
            .filter(|value| !value.is_null())
            .cloned(),
        cooldown,
        model_cooldowns: cooldowns
            .iter()
            .filter(|cooldown| scope(cooldown, "model"))
            .count(),
        quota: entry
            .get("quota")
            .filter(|value| !value.is_null() && value.as_object().is_none_or(|map| !map.is_empty()))
            .cloned(),
        id: text_field(entry, "id").unwrap_or_default(),
        raw_name,
    }
}

/// The running server, for a command that needs it.
async fn server(ctx: &Context) -> Result<Server, Failure> {
    let target = probe(ctx).await?;
    target.server()?;
    match target.reach {
        super::target::Reach::Running(server) => Ok(server),
        _ => Err(Failure::new(
            "not_running",
            "no server is running for this config",
        )),
    }
}

/// Every credential the running server has.
async fn fetch(server: &Server) -> Result<Vec<Credential>, Failure> {
    let listed = server
        .remote
        .json(Method::GET, "/v0/management/auth-files", None)
        .await?;
    let mut credentials: Vec<Credential> = listed
        .get("files")
        .and_then(Value::as_array)
        .map(|files| files.iter().map(credential).collect())
        .unwrap_or_default();
    let reply = server
        .remote
        .send(Method::GET, "/open-ferry/api/v1/claude-cli/entries", None)
        .await?;
    if reply.status == 200 {
        let entries: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
        for entry in entries
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            add_claude_cli(&mut credentials, entry);
        }
    }
    Ok(credentials)
}

/// Adds the `claude-cli` entry `entry` to `credentials`, or marks the
/// credential it loaded as one.
fn add_claude_cli(credentials: &mut Vec<Credential>, entry: &Value) {
    let loaded = entry.get("credential").filter(|value| value.is_object());
    if let Some(loaded) = loaded {
        let index = text_field(loaded, "auth_index");
        if let Some(listed) = credentials
            .iter_mut()
            .find(|listed| index.is_some() && listed.auth_index == index)
        {
            listed.source = "claude-cli".to_owned();
            return;
        }
        let mut made = credential(loaded);
        made.source = "claude-cli".to_owned();
        credentials.push(made);
        return;
    }
    let name = text_field(entry, "name").unwrap_or_default();
    let disabled = entry
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    credentials.push(Credential {
        name: mask_plain(&name),
        auth_index: None,
        provider: "claude-cli".to_owned(),
        label: text_field(entry, "prefix"),
        state: if disabled { "off" } else { "unknown" },
        status: None,
        status_message: text_field(entry, "last_error").map(|message| mask_plain(&message)),
        disabled,
        source: "claude-cli".to_owned(),
        account: None,
        priority: None,
        cooldown: None,
        model_cooldowns: 0,
        quota: None,
        id: String::new(),
        raw_name: name,
    });
}

/// The credentials, as listed.
#[derive(Debug, Serialize)]
struct Listed {
    count: usize,
    counts: BTreeMap<&'static str, usize>,
    credentials: Vec<Credential>,
}

impl Report for Listed {
    fn text(&self) -> String {
        if self.credentials.is_empty() {
            return "No credentials.\n".to_owned();
        }
        let mut out = String::new();
        for credential in &self.credentials {
            let index = credential.auth_index.as_deref().unwrap_or("-");
            let mut line = format!(
                "{:<10} {:<12} {:<18} {}",
                credential.state, credential.provider, index, credential.name
            );
            if let Some(account) = &credential.account {
                line.push_str(&format!("  {account}"));
            }
            if let Some(cooldown) = &credential.cooldown {
                if let Some(retry) = cooldown.get("retry_at").and_then(Value::as_str) {
                    line.push_str(&format!("  (resting until {retry})"));
                }
            } else if let Some(message) = &credential.status_message
                && credential.state != "ready"
            {
                line.push_str(&format!("  ({message})"));
            }
            if credential.model_cooldowns > 0 {
                line.push_str(&format!(
                    "  [{} model(s) cooling down]",
                    credential.model_cooldowns
                ));
            }
            out.push_str(&line);
            out.push('\n');
        }
        let counts: Vec<String> = self
            .counts
            .iter()
            .map(|(state, count)| format!("{count} {state}"))
            .collect();
        out.push_str(&format!(
            "{} credential(s): {}\n",
            self.count,
            counts.join(", ")
        ));
        out
    }
}

/// How many of `credentials` are in each state.
pub(crate) fn count_states(credentials: &[Credential]) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for credential in credentials {
        *counts.entry(credential.state).or_insert(0) += 1;
    }
    counts
}

/// Every credential of the server running for `ctx`'s config, for
/// `status`.
pub(crate) async fn all(server: &Server) -> Result<Vec<Credential>, Failure> {
    fetch(server).await
}

/// `credentials list`.
pub(crate) async fn list(ctx: &Context, input: &ListInput) -> Result<Outcome, Failure> {
    if let Some(state) = &input.state
        && !STATES.contains(&state.as_str())
    {
        return Err(Failure::usage(format!(
            "unknown state {state}: one of {}",
            STATES.join(", ")
        )));
    }
    let server = server(ctx).await?;
    let credentials: Vec<Credential> = fetch(&server)
        .await?
        .into_iter()
        .filter(|credential| {
            input
                .state
                .as_deref()
                .is_none_or(|state| credential.state == state)
        })
        .filter(|credential| {
            input
                .provider
                .as_deref()
                .is_none_or(|provider| credential.provider.eq_ignore_ascii_case(provider.trim()))
        })
        .collect();
    Ok(Outcome::of(&Listed {
        count: credentials.len(),
        counts: count_states(&credentials),
        credentials,
    }))
}

/// The one credential `name` names: by `auth_index`, ID, file name or
/// name as listed.
fn find(credentials: Vec<Credential>, name: &str, ctx: &Context) -> Result<Credential, Failure> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Failure::usage(
            "name a credential by its auth_index or name, as listed",
        ));
    }
    let by_index: Vec<&Credential> = credentials
        .iter()
        .filter(|credential| credential.auth_index.as_deref() == Some(name))
        .collect();
    let matches: Vec<&Credential> = if by_index.is_empty() {
        credentials
            .iter()
            .filter(|credential| {
                credential.id == name || credential.raw_name == name || credential.name == name
            })
            .collect()
    } else {
        by_index
    };
    let hint = format!("list them with `{}`", ctx.command_name("credentials list"));
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => {
            Err(Failure::new("not_found", "no credential has that auth_index or name").hint(hint))
        }
        _ => Err(Failure::new(
            "ambiguous",
            "more than one credential has that name; name it by its auth_index",
        )
        .hint(hint)),
    }
}

/// What a command on one credential did.
#[derive(Debug, Serialize)]
struct Done {
    action: &'static str,
    credential: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_index: Option<String>,
    changed: bool,
    #[serde(flatten)]
    extra: Map<String, Value>,
    #[serde(skip)]
    summary: String,
}

impl Report for Done {
    fn text(&self) -> String {
        format!("{}\n", self.summary)
    }
}

/// `credentials enable` and `credentials disable`.
pub(crate) async fn set_disabled(
    ctx: &Context,
    input: &TargetInput,
    disabled: bool,
) -> Result<Outcome, Failure> {
    let server = server(ctx).await?;
    let found = find(fetch(&server).await?, &input.credential, ctx)?;
    let action = if disabled { "disable" } else { "enable" };
    if found.source == "claude-cli" {
        return Err(Failure::new(
            "refused",
            format!(
                "{} is a claude-cli entry, which is turned off and on in the config",
                found.name
            ),
        )
        .hint(format!(
            "set its disabled in the claude-cli list: read it with `{}`, then set the list whole",
            ctx.command_name("config get claude-cli")
        )));
    }
    if found.disabled == disabled {
        return Ok(Outcome::of(&Done {
            action,
            credential: found.name.clone(),
            auth_index: found.auth_index.clone(),
            changed: false,
            extra: Map::new(),
            summary: format!("{} is {}d already; nothing to change.", found.name, action),
        }));
    }
    let name = if found.id.is_empty() {
        found.raw_name.clone()
    } else {
        found.id.clone()
    };
    server
        .remote
        .json(
            Method::PATCH,
            "/v0/management/auth-files/status",
            Some(Body::Json(json!({
                "name": name,
                "auth_index": found.auth_index.clone().unwrap_or_default(),
                "disabled": disabled,
            }))),
        )
        .await?;
    let reverse = if disabled {
        "credentials enable"
    } else {
        "credentials disable"
    };
    let index = found
        .auth_index
        .clone()
        .unwrap_or_else(|| found.name.clone());
    let undo = match ctx.caller {
        Caller::Cli => format!("Reverse it with `open-ferry {reverse} {index}`."),
        Caller::Mcp => format!("Reverse it with the {} tool.", reverse.replace(' ', "_")),
    };
    let mut extra = Map::new();
    extra.insert("disabled".to_owned(), json!(disabled));
    extra.insert("undo".to_owned(), json!(undo));
    Ok(Outcome::of(&Done {
        action,
        credential: found.name.clone(),
        auth_index: found.auth_index.clone(),
        changed: true,
        extra,
        summary: format!("{} is {}d.\n{undo}", found.name, action),
    }))
}

/// `credentials reset-quota`.
pub(crate) async fn reset_quota(ctx: &Context, input: &TargetInput) -> Result<Outcome, Failure> {
    let server = server(ctx).await?;
    let found = find(fetch(&server).await?, &input.credential, ctx)?;
    let Some(index) = found.auth_index.clone() else {
        return Err(Failure::new(
            "refused",
            format!("{} isn't loaded, so it has no quota to reset", found.name),
        ));
    };
    let answer = server
        .remote
        .json(
            Method::POST,
            "/v0/management/reset-quota",
            Some(Body::Json(json!({"auth_index": index}))),
        )
        .await?;
    let models = answer.get("models").cloned().unwrap_or_else(|| json!([]));
    let count = models.as_array().map_or(0, Vec::len);
    let mut extra = Map::new();
    extra.insert("models".to_owned(), models);
    Ok(Outcome::of(&Done {
        action: "reset_quota",
        credential: found.name.clone(),
        auth_index: Some(index),
        changed: true,
        extra,
        summary: format!(
            "Reset the quota state of {}: it can be picked again at once{}.",
            found.name,
            if count > 0 {
                format!(", for {count} model(s) too")
            } else {
                String::new()
            }
        ),
    }))
}

/// `credentials remove`.
pub(crate) async fn remove(ctx: &Context, input: &TargetInput) -> Result<Outcome, Failure> {
    let server = server(ctx).await?;
    let found = find(fetch(&server).await?, &input.credential, ctx)?;
    if found.source != "file" {
        return Err(Failure::new(
            "refused",
            format!(
                "{} doesn't come from a credential file, so there is no file to remove; a credential from the config is removed from the config",
                found.name
            ),
        ));
    }
    confirm(
        ctx,
        &format!("Removing the credential {}", found.name),
        &["it deletes the credential's file, which `config undo` doesn't bring back".to_owned()],
        json!({"credential": found.name, "auth_index": found.auth_index}),
    )?;
    server
        .remote
        .json(
            Method::DELETE,
            "/v0/management/auth-files",
            Some(Body::Json(json!({"name": found.raw_name}))),
        )
        .await?;
    Ok(Outcome::of(&Done {
        action: "remove",
        credential: found.name.clone(),
        auth_index: found.auth_index.clone(),
        changed: true,
        extra: Map::new(),
        summary: format!(
            "Removed {}: its file is deleted, and the server stops using it.",
            found.name
        ),
    }))
}

/// What a sign-in did.
#[derive(Debug, Serialize)]
struct LoggedIn {
    provider: String,
    /// `wait` (open `url`, then call again with `state`), `ok` or
    /// `error`.
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip)]
    summary: String,
}

impl Report for LoggedIn {
    fn text(&self) -> String {
        format!("{}\n", self.summary)
    }
}

/// The login route of `provider`, and its name for people.
fn login_route(ctx: &Context, provider: &str) -> Result<(&'static str, &'static str), Failure> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "codex" | "openai" => Ok(("/v0/management/codex-auth-url", "Codex")),
        "claude" | "anthropic" => {
            confirm(
                ctx,
                "Signing in to Claude",
                &["Anthropic's terms don't allow a tool such as open-ferry to sign in to a Claude subscription and use its tokens (see docs/claude-subscription.md): such a sign-in gets only the Haiku models, and may put the account at risk; claude-cli, which runs your own Claude Code, is the way to use a subscription".to_owned()],
                json!({}),
            )?;
            Ok(("/v0/management/anthropic-auth-url", "Claude"))
        }
        "claude-cli" => Err(Failure::new(
            "refused",
            "a claude-cli entry uses Claude Code's own sign-in, which open-ferry never runs or reads",
        )
        .hint("run `claude auth login` yourself in a terminal, then add an entry to the claude-cli list (see docs/claude-subscription.md)")),
        _ => Err(Failure::usage(format!(
            "{} has no sign-in here: one of codex and claude",
            provider.trim()
        ))),
    }
}

/// `credentials login`.
pub(crate) async fn login(ctx: &Context, input: &LoginInput) -> Result<Outcome, Failure> {
    if let Some(state) = input.state.as_deref() {
        let server = server(ctx).await?;
        return follow(ctx, &server, &input.provider, state, None).await;
    }
    let (route, name) = login_route(ctx, &input.provider)?;
    let server = server(ctx).await?;
    let started = server
        .remote
        .json(Method::GET, &format!("{route}?is_webui=true"), None)
        .await?;
    let url = text_field(&started, "url")
        .ok_or_else(|| Failure::new("failed", "the server's answer has no sign-in address"))?;
    let state = text_field(&started, "state")
        .ok_or_else(|| Failure::new("failed", "the server's answer has no sign-in state"))?;
    let wait = input.wait && ctx.caller == Caller::Cli;
    if !wait {
        let again = match ctx.caller {
            Caller::Cli => format!(
                "Then run `open-ferry credentials login {} --state {state}` to wait for it.",
                input.provider.trim()
            ),
            Caller::Mcp => format!(
                "Give the person this address to open, then call credentials_login again with state \"{state}\" to wait for it."
            ),
        };
        return Ok(Outcome::of(&LoggedIn {
            provider: input.provider.trim().to_owned(),
            status: "wait".to_owned(),
            url: Some(url.clone()),
            state: Some(state.clone()),
            error: None,
            summary: format!(
                "Open this address in a browser to sign in to {name}:\n  {url}\n{again}"
            ),
        }));
    }
    if let Some(say) = &ctx.say {
        say(&format!(
            "Open this address in a browser to sign in to {name}:\n  {url}\nWaiting for the sign-in (Ctrl-C to stop)...\n"
        ));
    }
    let followed = tokio::select! {
        followed = follow(ctx, &server, &input.provider, &state, Some(url)) => followed,
        _ = tokio::signal::ctrl_c() => {
            let _ = server
                .remote
                .send(Method::DELETE, &format!("/v0/management/oauth-session?state={}", encode(&state)), None)
                .await;
            return Err(Failure::new("cancelled", "Stopped; the sign-in was cancelled."));
        }
    };
    followed
}

/// Waits for the sign-in `state` to finish.
async fn follow(
    ctx: &Context,
    server: &Server,
    provider: &str,
    state: &str,
    url: Option<String>,
) -> Result<Outcome, Failure> {
    let (poll, limit) = match ctx.caller {
        Caller::Cli => (CLI_POLL, CLI_WAIT),
        Caller::Mcp => (MCP_POLL, MCP_WAIT),
    };
    let started = tokio::time::Instant::now();
    let path = format!(
        "/v0/management/get-auth-status?state={}",
        encode(state.trim())
    );
    loop {
        let answer = server.remote.json(Method::GET, &path, None).await?;
        let status = text_field(&answer, "status").unwrap_or_default();
        match status.as_str() {
            "ok" => {
                return Ok(Outcome::of(&LoggedIn {
                    provider: provider.trim().to_owned(),
                    status: "ok".to_owned(),
                    url: None,
                    state: Some(state.to_owned()),
                    error: None,
                    summary: format!(
                        "Signed in: the credential is saved, and the server uses it. See it with `{}`.",
                        ctx.command_name("credentials list")
                    ),
                }));
            }
            "error" => {
                let error = text_field(&answer, "error").unwrap_or_else(|| "it failed".to_owned());
                return Err(Failure::new(
                    "login_failed",
                    format!("the sign-in didn't finish: {}", mask_plain(&error)),
                ));
            }
            _ => {}
        }
        if started.elapsed() + poll > limit {
            let again = match ctx.caller {
                Caller::Cli => format!(
                    "run `open-ferry credentials login {} --state {state}` to wait again",
                    provider.trim()
                ),
                Caller::Mcp => {
                    format!("call credentials_login again with state \"{state}\" to wait again")
                }
            };
            return Ok(Outcome::of(&LoggedIn {
                provider: provider.trim().to_owned(),
                status: "wait".to_owned(),
                url,
                state: Some(state.to_owned()),
                error: None,
                summary: format!("The sign-in hasn't finished yet; {again}."),
            })
            .code(super::exit::FAILED));
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: each state a credential's entry maps to.
    #[test]
    fn maps_states() {
        let state = |entry: Value| state_of(&entry);
        assert_eq!(state(json!({"status": "active"})), "ready");
        assert_eq!(state(json!({"status": "active", "disabled": true})), "off");
        assert_eq!(state(json!({"status": "disabled"})), "off");
        assert_eq!(state(json!({"status": "error"})), "failing");
        assert_eq!(state(json!({"status": "refreshing"})), "refreshing");
        assert_eq!(state(json!({"status": "pending"})), "waiting");
        assert_eq!(
            state(json!({"status": "active", "unavailable": true})),
            "resting"
        );
        assert_eq!(
            state(json!({"status": "active", "cooldowns": [{"scope": "credential"}]})),
            "resting"
        );
        assert_eq!(
            state(json!({"status": "active", "cooldowns": [{"scope": "model"}]})),
            "ready"
        );
        assert_eq!(state(json!({"status": "unknown"})), "unknown");
    }

    // Not upstream's: a listed credential's email addresses are masked, and
    // its cooldowns summed up.
    #[test]
    fn lists_a_credential() {
        let listed = credential(&json!({
            "id": "codex-someone@example.com.json",
            "name": "codex-someone@example.com.json",
            "auth_index": "3f2a",
            "provider": "codex",
            "email": "someone@example.com",
            "status": "active",
            "source": "file",
            "cooldowns": [
                {"scope": "credential", "reason": "quota", "retry_at": "2026-01-01T00:00:00Z", "remaining_seconds": 5},
                {"scope": "model", "model_key": "m"},
            ],
        }));
        assert!(
            !listed.name.contains("someone@example.com"),
            "{}",
            listed.name
        );
        assert!(!listed.account.as_deref().unwrap().contains("someone@"));
        assert_eq!(listed.state, "resting");
        assert_eq!(listed.model_cooldowns, 1);
        assert_eq!(listed.cooldown.unwrap()["reason"], "quota");
    }
}
