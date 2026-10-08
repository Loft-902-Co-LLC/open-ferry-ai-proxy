//! Making a change to the config: working out what it would change,
//! asking for a confirmation when it needs one, then making it through
//! the running server or, when none can be called, in the file, and
//! reporting each setting's old and new value, masked.
//!
//! A change is worked out from the file with the checks the v8 management
//! API's writes have, so one the server would refuse is refused before
//! anything is asked or written. While a server runs for the config and
//! takes the management key, the change goes through its management API,
//! which makes it under the lock every management write takes, so a change
//! here and one from the dashboard or another management client don't
//! lose each other. Otherwise it is written to the file with the writer
//! the server uses, which keeps the file it replaces as `<config>.bak`, as
//! the server's writes do, so `config undo` reverses either.
//!
//! A change is made only to the file it was worked out from. When the
//! file changed after it was read (another write, or a hand edit), a
//! change a person was asked about at the terminal is refused, and says
//! to run it again; any other is worked out again from the file as it is.

use std::path::Path;

use axum::http::Method;
use open_ferry_core::config::save::{self, SaveErrorKind};
use open_ferry_core::config::v8_edit::{V8Edit, V8EditError, V8Method, preview_v8};
use open_ferry_dashboard::mask_client_key;
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::api::{Body, path_segments};
use super::guard::{confirm, sensitive_reasons};
use super::mask::mask_at;
use super::target::{Reach, Server, probe};
use super::values::{Change, diff, placed, tree_of};
use super::{Caller, Context, Failure, Report};

/// What a change puts at a path, or the whole config.
#[derive(Clone)]
pub(crate) enum Content {
    /// Nothing: a removal.
    None,
    /// A value, as JSON.
    Json(Value),
    /// The whole config, as YAML.
    Yaml(Vec<u8>),
}

/// A change to the config, as the v8 management API takes one.
#[derive(Clone)]
pub(crate) struct Edit {
    pub(crate) method: V8Method,
    /// The keys leading to the value; empty for the whole config.
    pub(crate) parts: Vec<String>,
    pub(crate) content: Content,
}

impl Edit {
    /// The edit as the config writer takes it.
    fn v8(&self) -> V8Edit {
        let (body, yaml) = match &self.content {
            Content::None => (Vec::new(), false),
            Content::Json(value) => (value.to_string().into_bytes(), false),
            Content::Yaml(data) => (data.clone(), true),
        };
        V8Edit {
            method: self.method,
            path: self.parts.clone(),
            body,
            yaml,
        }
    }
}

/// How the running server is asked to make a change.
#[derive(Clone)]
pub(crate) enum Call {
    /// Through the v8 config routes, as the edit says.
    V8,
    /// Adding this client key, with `PATCH /v0/management/api-keys`.
    AddKey(String),
    /// Removing this client key, with `DELETE /v0/management/api-keys`.
    RemoveKey(String),
}

/// A change to make.
pub(crate) struct Request {
    pub(crate) edit: Edit,
    pub(crate) call: Call,
    /// What it is called in the report, such as `set`.
    pub(crate) action: &'static str,
    /// What it does, for people: `Setting routing.strategy`.
    pub(crate) what: String,
    /// The setting it changes, dotted.
    pub(crate) path: Option<String>,
    /// Why it needs a confirmation whatever it changes, if it does.
    pub(crate) always: Option<String>,
}

/// What a change did.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Changed {
    /// What it was: `set`, `unset`, `replace`, `undo`, `add_key` or
    /// `remove_key`.
    pub(crate) action: &'static str,
    /// The setting it changed, dotted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    /// Whether it changed anything.
    pub(crate) changed: bool,
    /// How: `server`, through the running server's management API, or
    /// `file`, in the config file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) via: Option<&'static str>,
    /// Each setting it changed, with its old and new value, masked.
    pub(crate) changes: Vec<Value>,
    /// Anything else to know.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) note: Option<String>,
    /// How to reverse it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) undo: Option<String>,
    /// What the command adds.
    #[serde(flatten)]
    pub(crate) extra: Map<String, Value>,
    /// The first line of the text.
    #[serde(skip)]
    pub(crate) summary: String,
    /// What the text ends with, if anything.
    #[serde(skip)]
    pub(crate) footer: Option<String>,
}

impl Changed {
    /// A change that changed nothing, because `why`.
    pub(crate) fn nothing(action: &'static str, path: Option<String>, why: String) -> Self {
        Self {
            action,
            path,
            changed: false,
            via: None,
            changes: Vec::new(),
            note: None,
            undo: None,
            extra: Map::new(),
            summary: why,
            footer: None,
        }
    }
}

impl Report for Changed {
    fn text(&self) -> String {
        let mut out = format!("{}\n", self.summary);
        for change in &self.changes {
            out.push_str(&super::values::change_line(change));
        }
        if let Some(note) = &self.note {
            out.push_str(note);
            out.push('\n');
        }
        if let Some(undo) = &self.undo {
            out.push_str(undo);
            out.push('\n');
        }
        if let Some(footer) = &self.footer {
            out.push_str(footer);
            out.push('\n');
        }
        out
    }
}

/// `changes`, each with its old and new value masked, as reported.
pub(crate) fn masked(changes: &[Change]) -> Vec<Value> {
    changes
        .iter()
        .map(|change| {
            let mut entry = Map::new();
            entry.insert("path".to_owned(), Value::String(change.path.clone()));
            if let Some(old) = &change.old {
                entry.insert("old".to_owned(), mask_at(&change.parts, old));
            }
            if let Some(new) = &change.new {
                entry.insert("new".to_owned(), mask_at(&change.parts, new));
            }
            Value::Object(entry)
        })
        .collect()
}

/// How `ctx`'s caller reverses a change.
pub(crate) fn undo_hint(ctx: &Context) -> String {
    match ctx.caller {
        Caller::Cli => "Undo it with `open-ferry config undo`.".to_owned(),
        Caller::Mcp => "Undo it with the config_undo tool.".to_owned(),
    }
}

/// The config file's contents.
pub(crate) fn read_config(path: &Path) -> Result<Vec<u8>, Failure> {
    std::fs::read(path).map_err(|error| {
        Failure::new(
            "not_found",
            format!("can't read {}: {error}", path.display()),
        )
    })
}

/// How many times a change is worked out again when the file changes
/// under it before it gives up.
const WORK_OUT_TRIES: usize = 3;

/// Works out `request`, asks for its confirmation if it needs one, makes
/// it, and says what it changed.
///
/// It is made only to the file it was worked out from. When the file
/// changed meanwhile, a change a person was shown and said yes to at the
/// terminal is refused; any other is worked out again from the file as it
/// is, with its checks and confirmation, a few times.
pub(crate) async fn make(ctx: &Context, request: Request) -> Result<Changed, Failure> {
    let mut data = read_config(&ctx.path)?;
    let mut shown = false;
    for _ in 0..WORK_OUT_TRIES {
        match attempt(ctx, &request, &data, &mut shown).await? {
            Attempt::Done(changed) => return Ok(changed),
            Attempt::Changed(_) if shown => return Err(config_changed()),
            Attempt::Changed(now) => data = now,
        }
    }
    Err(config_changed())
}

/// What one try at a change came to.
enum Attempt {
    /// It was made, or there was nothing to make.
    Done(Changed),
    /// The file changed after the change was worked out from it, and
    /// nothing was changed: the file as it is now.
    Changed(Vec<u8>),
}

/// Works out `request` from the config file's bytes `data` and makes it,
/// unless the file no longer holds them. `shown` is set once a person was
/// asked at the terminal.
async fn attempt(
    ctx: &Context,
    request: &Request,
    data: &[u8],
    shown: &mut bool,
) -> Result<Attempt, Failure> {
    let before = tree_of(data)?;
    let planned = preview_v8(data, &request.edit.v8()).map_err(edit_failure)?;
    let after = tree_of(&planned)?;
    let changes = diff(&before, &after);
    if changes.is_empty() {
        return Ok(Attempt::Done(Changed::nothing(
            request.action,
            request.path.clone(),
            "Nothing to change: the config already holds that.".to_owned(),
        )));
    }
    let mut reasons = sensitive_reasons(&changes, &before, &after);
    if let Some(always) = request.always.clone() {
        reasons.insert(0, always);
    }
    if !reasons.is_empty() {
        confirm(
            ctx,
            &request.what,
            &reasons,
            json!({"changes": masked(&changes)}),
        )?;
        *shown |= !ctx.yes;
    }
    let target = probe(ctx).await?;
    if target.data != data {
        return Ok(Attempt::Changed(target.data));
    }
    let (via, note) = match &target.reach {
        Reach::Running(server) => {
            call_server(server, request).await?;
            ("server", None)
        }
        Reach::Refused(failure) | Reach::OtherConfig(failure) => return Err(failure.clone()),
        other => {
            if let Err(error) = save::write_file_expecting(&ctx.path, &planned, data) {
                return match error.kind() {
                    SaveErrorKind::Stale => Ok(Attempt::Changed(read_config(&ctx.path)?)),
                    SaveErrorKind::Io | SaveErrorKind::Symlink => Err(Failure::new(
                        "failed",
                        format!(
                            "the config file couldn't be written, so nothing was changed: {error}"
                        ),
                    )),
                    _ => Err(Failure::new(
                        "failed",
                        "the change can't be written as a config file, so nothing was changed",
                    )),
                };
            }
            ("file", Some(file_note(other)))
        }
    };
    let now = tree_of(&read_config(&ctx.path)?)?;
    let changes = diff(&before, &now);
    let note = note.or_else(|| {
        let path = request.path.as_deref()?;
        let elsewhere = changes.iter().any(|change| {
            change.path != path && !change.path.starts_with(&format!("{path}."))
        });
        (via == "server" && elsewhere).then(|| {
            "The server's write also put settings it was using at their defaults into the file; that doesn't change what it does.".to_owned()
        })
    });
    Ok(Attempt::Done(Changed {
        action: request.action,
        path: request.path.clone(),
        changed: !changes.is_empty(),
        via: Some(via),
        changes: masked(&changes),
        note,
        undo: Some(undo_hint(ctx)),
        extra: Map::new(),
        summary: format!(
            "{}: done, {}.",
            request.what,
            match via {
                "server" => "through the running server",
                _ => "in the config file",
            }
        ),
        footer: None,
    }))
}

/// The failure for a config file that changed after a change to it was
/// worked out.
pub(crate) fn config_changed() -> Failure {
    Failure::new(
        "config_changed",
        "the config file changed after this change was worked out from it, so nothing was changed",
    )
    .hint("run it again to work it out from the file as it is now")
}

/// Why a change was written to the file, not made through a server.
pub(crate) fn file_note(reach: &Reach) -> String {
    match reach {
        Reach::NotRunning(why) => format!(
            "No server answers for this config ({why}), so the file was changed; a server running for it loads the change when it sees the file change, and one started later reads it."
        ),
        Reach::NoKey => "A server is running, but there is no management key to call it with, so the file was changed; the server loads the change when it sees the file change. Until it has, a save of its own settings (from the dashboard or the management API) writes the settings it holds over the file and can undo this change; `config get` reads what the file holds.".to_owned(),
        Reach::ManagementOff => "A server is running with its management API off, so the file was changed; the server loads the change when it sees the file change.".to_owned(),
        Reach::Running(_) | Reach::Refused(_) | Reach::OtherConfig(_) => String::new(),
    }
}

/// Asks `server` to make `request`.
async fn call_server(server: &Server, request: &Request) -> Result<(), Failure> {
    let remote = &server.remote;
    match &request.call {
        Call::V8 => {
            let edit = &request.edit;
            let path = format!("/v8/management/config/{}", path_segments(&edit.parts));
            match (&edit.content, edit.parts.is_empty()) {
                (Content::Yaml(data), true) => {
                    remote
                        .json(
                            Method::PUT,
                            "/v8/management/config.yaml",
                            Some(Body::Yaml(data.clone())),
                        )
                        .await?;
                }
                (Content::Json(value), false) => {
                    remote
                        .json(Method::PUT, &path, Some(Body::Json(value.clone())))
                        .await?;
                }
                (Content::None, false) => {
                    remote.json(Method::DELETE, &path, None).await?;
                }
                _ => {
                    return Err(Failure::new(
                        "failed",
                        "this change can't be sent to the server",
                    ));
                }
            }
        }
        Call::AddKey(key) => {
            remote
                .json(
                    Method::PATCH,
                    "/v0/management/api-keys",
                    Some(Body::Json(json!({"old": key, "new": key}))),
                )
                .await?;
        }
        Call::RemoveKey(key) => {
            let before = server_keys(remote).await?;
            let index = before
                .iter()
                .position(|listed| listed.as_str() == Some(key))
                .ok_or_else(|| {
                    Failure::new(
                        "not_found",
                        "the running server doesn't have that client key",
                    )
                })?;
            // By its index, as upstream's route takes it: `?value=` would
            // put the key in the URL. The list is read again after, as
            // another write between the two can move the key.
            remote
                .json(
                    Method::DELETE,
                    &format!("/v0/management/api-keys?index={index}"),
                    None,
                )
                .await?;
            let after = server_keys(remote).await.map_err(|failure| {
                Failure::new(
                    "key_list_changed",
                    format!(
                        "a client key was removed by its place in the server's list, but the list couldn't be read again to check it was this one: {}",
                        failure.message
                    ),
                )
                .hint(KEY_LIST_HINT)
            })?;
            let gone = removed(&before, &after);
            if gone.len() != 1 || gone.first().and_then(Value::as_str) != Some(key.as_str()) {
                return Err(key_list_changed(&gone));
            }
        }
    }
    Ok(())
}

/// What to do when a key removed through the server may not be the one
/// confirmed.
const KEY_LIST_HINT: &str = "look at the keys with `keys list`; `config undo` puts back the list as it was before that write";

/// The running server's client keys, as `GET /v0/management/api-keys`
/// lists them.
async fn server_keys(remote: &super::api::Remote) -> Result<Vec<Value>, Failure> {
    let listed = remote
        .json(Method::GET, "/v0/management/api-keys", None)
        .await?;
    Ok(listed
        .get("api-keys")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// The keys of `before` that aren't in `after`, each as often as it is
/// missing.
fn removed(before: &[Value], after: &[Value]) -> Vec<Value> {
    let mut left: Vec<&Value> = after.iter().collect();
    before
        .iter()
        .filter(|key| match left.iter().position(|other| other == key) {
            Some(found) => {
                left.swap_remove(found);
                false
            }
            None => true,
        })
        .cloned()
        .collect()
}

/// The failure for a removal by index that didn't remove just the key
/// confirmed, as when another write moved the keys meanwhile: `gone`, the
/// keys missing now, masked.
fn key_list_changed(gone: &[Value]) -> Failure {
    let message = if gone.is_empty() {
        "the server's client keys changed while this key was being removed, and it is still listed, so another key may have been removed in its place".to_owned()
    } else {
        let masked: Vec<String> = gone
            .iter()
            .map(|key| match key.as_str() {
                Some(key) => mask_client_key(key),
                None => "(not a string)".to_owned(),
            })
            .collect();
        format!(
            "the server's client keys changed while this key was being removed: the keys gone from its list now are {}, not just the one confirmed",
            masked.join(", ")
        )
    };
    Failure::new("key_list_changed", message).hint(KEY_LIST_HINT)
}

/// The failure for a change the config writer refused.
pub(crate) fn edit_failure(error: V8EditError) -> Failure {
    match error {
        V8EditError::ReadFailed => Failure::new("failed", "the config file can't be read"),
        V8EditError::StoredInvalid(message) => Failure::new(
            "invalid_config",
            placed(
                "the config file as it is doesn't read in the v8 layout",
                &message,
            ),
        ),
        V8EditError::CannotDeleteConfig => Failure::usage("the whole config can't be removed"),
        V8EditError::NotFound => Failure::new("not_found", "that setting isn't set"),
        V8EditError::InvalidBody | V8EditError::InvalidJson => {
            Failure::new("invalid_value", "the value isn't YAML or JSON")
        }
        V8EditError::ConfigMustBeObject => Failure::new(
            "invalid_value",
            "a whole config must be a mapping of settings",
        ),
        V8EditError::InvalidPath => Failure::new(
            "invalid_path",
            "a setting on the way to that one isn't a mapping, so nothing can go under it",
        ),
        V8EditError::InvalidConfig(message) | V8EditError::Unprocessable(message) => Failure::new(
            "invalid_value",
            placed("that would make the config invalid", &message),
        ),
        V8EditError::ReadOnlyField(field) => {
            Failure::new("read_only", format!("{field} can't be changed this way"))
        }
        V8EditError::WriteFailed(message) => Failure::new(
            "failed",
            format!("the config file couldn't be written: {message}"),
        ),
        other => Failure::new(
            "failed",
            placed("the config writer refused the change", &other.to_string()),
        ),
    }
}
