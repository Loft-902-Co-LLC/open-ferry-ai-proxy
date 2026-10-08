//! `open-ferry config`: reading a setting, setting and unsetting one,
//! showing the whole config masked, comparing it with its backup, undoing
//! the last change, and replacing the whole config.
//!
//! Paths are a v8 config's keys (`routing.strategy`); an unknown one is
//! refused with the nearest known one. A value is YAML or JSON. A value
//! that holds a secret (by its key, as [`super::mask`] tells one) is never
//! taken as an argument: it is read from standard input or a file.

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use axum::http::Method;
use open_ferry_core::config::save::{self, SaveErrorKind};
use open_ferry_core::config::v8_edit::{KnownKind, V8Method, known_v8_paths};
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::api::answer_failure;
use super::change::{Call, Changed, Content, Edit, Request, file_note, make, masked};
use super::guard::{confirm, sensitive_reasons};
use super::mask::{holds_secret, is_secret_name, mask_at, mask_tree};
use super::target::{Reach, probe};
use super::values::{
    check_path, diff as diff_trees, dotted, get as get_value, parse_value, read_tree,
    show as show_value, split_path, to_yaml, tree_of,
};
use super::{Caller, Context, Failure, Outcome, Report};

/// The most read from a file a value or config comes from.
const FILE_LIMIT: u64 = 1024 * 1024;

/// Where a value comes from.
#[derive(Clone)]
pub(crate) enum Source {
    /// An argument on the command line: never a secret.
    Argument(String),
    /// A tool call's JSON: never a secret.
    Json(Value),
    /// Standard input, read.
    Stdin(String),
    /// A file, to read.
    File(PathBuf),
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Argument(_) => f.write_str("Argument(..)"),
            Self::Json(_) => f.write_str("Json(..)"),
            Self::Stdin(_) => f.write_str("Stdin(..)"),
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
        }
    }
}

impl Source {
    /// Whether the value is in the call itself, where a secret mustn't be.
    pub(crate) fn is_inline(&self) -> bool {
        matches!(self, Self::Argument(_) | Self::Json(_))
    }

    /// The value's text, as given.
    pub(crate) fn text(&self) -> Result<String, Failure> {
        match self {
            Self::Argument(text) | Self::Stdin(text) => Ok(text.clone()),
            Self::Json(value) => Ok(match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            }),
            Self::File(path) => read_file(path),
        }
    }

    /// The value's text, as given, as bytes.
    fn bytes(&self) -> Result<Vec<u8>, Failure> {
        self.text().map(String::into_bytes)
    }
}

/// The text of the file at `path`, of up to [`FILE_LIMIT`] bytes.
pub(crate) fn read_file(path: &Path) -> Result<String, Failure> {
    let unreadable =
        |why: String| Failure::new("not_found", format!("can't read {}: {why}", path.display()));
    let file = std::fs::File::open(path).map_err(|error| unreadable(error.to_string()))?;
    let mut data = Vec::new();
    file.take(FILE_LIMIT + 1)
        .read_to_end(&mut data)
        .map_err(|error| unreadable(error.to_string()))?;
    if u64::try_from(data.len()).unwrap_or(u64::MAX) > FILE_LIMIT {
        return Err(unreadable("it is bigger than 1 MiB".to_owned()));
    }
    String::from_utf8(data).map_err(|_| unreadable("it isn't UTF-8".to_owned()))
}

/// `config get`'s input.
#[derive(Clone, Debug)]
pub(crate) struct GetInput {
    pub(crate) path: String,
}

/// `config set`'s input.
#[derive(Clone, Debug)]
pub(crate) struct SetInput {
    pub(crate) path: String,
    pub(crate) value: Source,
    /// Take the value as a string, as it is, not as YAML.
    pub(crate) string: bool,
}

/// `config unset`'s input.
#[derive(Clone, Debug)]
pub(crate) struct UnsetInput {
    pub(crate) path: String,
}

/// `config replace`'s input.
#[derive(Clone, Debug)]
pub(crate) struct ReplaceInput {
    pub(crate) source: Source,
}

/// A setting's value, or that it isn't set and its default.
#[derive(Debug, Serialize)]
struct Got {
    path: String,
    set: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<Value>,
}

impl Report for Got {
    fn text(&self) -> String {
        match (&self.value, &self.default) {
            (Some(value), _) => block(&self.path, value),
            (None, Some(default)) => format!(
                "{} is not set; the default is {}",
                self.path,
                block_value(default)
            ),
            (None, None) => format!(
                "{} is not set, and has no default: unset, it is empty or off\n",
                self.path
            ),
        }
    }
}

/// `path: value`, the value on its own lines when it is a mapping or a
/// list.
fn block(path: &str, value: &Value) -> String {
    format!("{path}: {}", block_value(value))
}

fn block_value(value: &Value) -> String {
    match value {
        Value::Object(map) if !map.is_empty() => format!("\n{}", indent(&to_yaml(value))),
        Value::Array(items) if !items.is_empty() => format!("\n{}", indent(&to_yaml(value))),
        other => format!("{}\n", show_value(other)),
    }
}

fn indent(text: &str) -> String {
    text.lines().map(|line| format!("  {line}\n")).collect()
}

/// `config get`.
pub(crate) fn get(ctx: &Context, input: &GetInput) -> Result<Outcome, Failure> {
    let parts = split_path(&input.path);
    check_path(&parts)?;
    let tree = read_tree(&ctx.path)?;
    let path = dotted(&parts);
    let got = match get_value(&tree, &parts) {
        Some(value) => Got {
            path,
            set: true,
            value: Some(mask_at(&parts, value)),
            default: None,
        },
        None => Got {
            path,
            set: false,
            value: None,
            default: default_of(&parts)?.map(|value| mask_at(&parts, &value)),
        },
    };
    Ok(Outcome::of(&got))
}

/// The default of the setting at `parts`, when it has one that isn't
/// empty.
fn default_of(parts: &[String]) -> Result<Option<Value>, Failure> {
    let defaults = save::defaults()
        .map_err(|error| Failure::new("failed", format!("the defaults can't be read: {error}")))?;
    let borrowed: Vec<&str> = parts.iter().map(String::as_str).collect();
    let value = match defaults.value(&borrowed) {
        Some(Ok(value)) => super::values::any_to_json(&value),
        _ => return Ok(None),
    };
    let empty = match &value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    };
    Ok((!empty).then_some(value))
}

/// The kind of setting `parts` names, when it is a known one.
fn kind_of(parts: &[String]) -> Option<KnownKind> {
    let path = dotted(parts);
    known_v8_paths()
        .into_iter()
        .find(|entry| entry.path == path)
        .map(|entry| entry.kind)
}

/// How a secret is given instead of as an argument.
fn secret_hint(ctx: &Context) -> &'static str {
    match ctx.caller {
        Caller::Cli => {
            "pipe it in with --from-stdin, or name a file that holds it with --from-file"
        }
        Caller::Mcp => "name a file that holds it with from_file",
    }
}

/// The failure for a secret given in the call.
pub(crate) fn secret_in_argument(ctx: &Context, what: &str) -> Failure {
    Failure::new(
        "secret_in_argument",
        format!("{what} is a secret, and a secret is never taken as an argument, where it would be seen and kept in the shell's history or the agent's transcript; nothing was changed"),
    )
    .hint(secret_hint(ctx))
}

/// `config set`.
pub(crate) async fn set(ctx: &Context, input: SetInput) -> Result<Outcome, Failure> {
    let parts = split_path(&input.path);
    check_path(&parts)?;
    let path = dotted(&parts);
    let secret_key = parts.last().is_some_and(|key| is_secret_name(key));
    let list = kind_of(&parts) == Some(KnownKind::List);
    let value = match &input.value {
        Source::Json(value) if !input.string => value.clone(),
        source => {
            let text = source.text()?;
            let text = if source.is_inline() {
                text
            } else {
                text.trim_end_matches(['\r', '\n']).to_owned()
            };
            // A secret's one line is taken as a string, so a key isn't read
            // as YAML; but not a JSON list or object, as a list of keys is.
            let structured = serde_json::from_str::<Value>(text.trim())
                .is_ok_and(|value| value.is_array() || value.is_object());
            if input.string || (secret_key && !list && !structured && !text.contains('\n')) {
                Value::String(if input.string {
                    text
                } else {
                    text.trim().to_owned()
                })
            } else {
                parse_value(&text)?
            }
        }
    };
    if input.value.is_inline() && holds_secret(&parts, &value) {
        return Err(secret_in_argument(ctx, &path));
    }
    let changed = make(
        ctx,
        Request {
            edit: Edit {
                method: V8Method::Put,
                parts,
                content: Content::Json(value),
            },
            call: Call::V8,
            action: "set",
            what: format!("Setting {path}"),
            path: Some(path),
            always: None,
        },
    )
    .await?;
    Ok(Outcome::of(&changed))
}

/// `config unset`.
pub(crate) async fn unset(ctx: &Context, input: &UnsetInput) -> Result<Outcome, Failure> {
    let parts = split_path(&input.path);
    check_path(&parts)?;
    let path = dotted(&parts);
    let tree = read_tree(&ctx.path)?;
    if get_value(&tree, &parts).is_none() {
        let changed = Changed::nothing(
            "unset",
            Some(path.clone()),
            format!("{path} is not set; nothing to change."),
        );
        return Ok(Outcome::of(&changed));
    }
    let changed = make(
        ctx,
        Request {
            edit: Edit {
                method: V8Method::Delete,
                parts,
                content: Content::None,
            },
            call: Call::V8,
            action: "unset",
            what: format!("Unsetting {path}"),
            path: Some(path),
            always: None,
        },
    )
    .await?;
    Ok(Outcome::of(&changed))
}

/// The whole config, masked.
#[derive(Debug, Serialize)]
struct Shown {
    config: String,
    settings: Value,
}

impl Report for Shown {
    fn text(&self) -> String {
        format!("# {}, masked\n{}", self.config, to_yaml(&self.settings))
    }
}

/// `config show`.
pub(crate) fn show(ctx: &Context) -> Result<Outcome, Failure> {
    let tree = read_tree(&ctx.path)?;
    Ok(Outcome::of(&Shown {
        config: ctx.path.display().to_string(),
        settings: mask_tree(&tree),
    }))
}

/// The settings the config changed since its backup.
#[derive(Debug, Serialize)]
struct Diffed {
    config: String,
    backup: String,
    changes: Vec<Value>,
}

impl Report for Diffed {
    fn text(&self) -> String {
        if self.changes.is_empty() {
            return format!(
                "{} holds the same settings as its backup, {}.\n",
                self.config, self.backup
            );
        }
        let mut out = format!(
            "What the last change made, from the backup {} to {}:\n",
            self.backup, self.config
        );
        for change in &self.changes {
            out.push_str(&super::values::change_line(change));
        }
        out
    }
}

/// The failure when the config has no backup.
fn no_backup(backup: &Path) -> Failure {
    Failure::new(
        "no_backup",
        format!(
            "there is no backup of the config, {}: nothing has changed it here yet",
            backup.display()
        ),
    )
}

/// `config diff`.
pub(crate) fn diff(ctx: &Context) -> Result<Outcome, Failure> {
    let backup = save::backup_path(&ctx.path);
    if !backup.exists() {
        return Err(no_backup(&backup));
    }
    let current = read_tree(&ctx.path)?;
    let before = read_tree(&backup)?;
    Ok(Outcome::of(&Diffed {
        config: ctx.path.display().to_string(),
        backup: backup.display().to_string(),
        changes: masked(&diff_trees(&before, &current)),
    }))
}

/// How `ctx`'s caller redoes what an undo undid.
fn redo_hint(ctx: &Context) -> String {
    match ctx.caller {
        Caller::Cli => "Run `open-ferry config undo` again to redo it.".to_owned(),
        Caller::Mcp => "Call config_undo again to redo it.".to_owned(),
    }
}

/// `config undo`.
pub(crate) async fn undo(ctx: &Context) -> Result<Outcome, Failure> {
    let backup = save::backup_path(&ctx.path);
    if !backup.exists() {
        return Err(no_backup(&backup));
    }
    let before = read_tree(&ctx.path)?;
    let after = tree_of(&std::fs::read(&backup).map_err(|error| {
        Failure::new(
            "failed",
            format!("can't read {}: {error}", backup.display()),
        )
    })?)?;
    let changes = diff_trees(&before, &after);
    let reasons = sensitive_reasons(&changes, &before, &after);
    if !reasons.is_empty() {
        confirm(
            ctx,
            "Undoing the last change",
            &reasons,
            json!({"changes": masked(&changes)}),
        )?;
    }
    let target = probe(ctx).await?;
    let mut note = None;
    let via = match &target.reach {
        Reach::Running(server) => {
            let reply = server
                .remote
                .send(Method::POST, "/open-ferry/api/v1/config/undo", None)
                .await?;
            match reply.status {
                200..=299 => "server",
                404 => {
                    undo_in_file(ctx, &backup)?;
                    note = Some("The running server has no dashboard API to undo through, so the file was changed; the server loads the change when it sees the file change.".to_owned());
                    "file"
                }
                409 => return Err(no_backup(&backup)),
                status => return Err(answer_failure(status, &reply.body)),
            }
        }
        Reach::Refused(failure) | Reach::OtherConfig(failure) => return Err(failure.clone()),
        other => {
            undo_in_file(ctx, &backup)?;
            note = Some(file_note(other));
            "file"
        }
    };
    let now = read_tree(&ctx.path)?;
    let changes = diff_trees(&before, &now);
    let changed = Changed {
        action: "undo",
        path: None,
        changed: !changes.is_empty(),
        via: Some(via),
        changes: masked(&changes),
        note,
        undo: Some(redo_hint(ctx)),
        extra: Map::new(),
        summary: format!(
            "Undid the last change: {}.",
            if via == "server" {
                "through the running server"
            } else {
                "in the config file"
            }
        ),
        footer: None,
    };
    Ok(Outcome::of(&changed))
}

/// Undoes the last change in the file.
fn undo_in_file(ctx: &Context, backup: &Path) -> Result<(), Failure> {
    let check = save::UndoCheck {
        force: true,
        ..save::UndoCheck::default()
    };
    save::undo(&ctx.path, &check)
        .map(|_| ())
        .map_err(|error| match error.kind() {
            SaveErrorKind::NoBackup => no_backup(backup),
            _ => Failure::new("failed", format!("the undo failed: {error}")),
        })
}

/// `config replace`.
pub(crate) async fn replace(ctx: &Context, input: ReplaceInput) -> Result<Outcome, Failure> {
    if input.source.is_inline() {
        return Err(Failure::usage(
            "a whole config is read from standard input or a file, not given as an argument",
        )
        .hint(secret_hint(ctx)));
    }
    let data = input.source.bytes()?;
    let changed = make(
        ctx,
        Request {
            edit: Edit {
                method: V8Method::Put,
                parts: Vec::new(),
                content: Content::Yaml(data),
            },
            call: Call::V8,
            action: "replace",
            what: "Replacing the whole config".to_owned(),
            path: None,
            always: Some("it replaces the whole config".to_owned()),
        },
    )
    .await?;
    Ok(Outcome::of(&changed))
}
