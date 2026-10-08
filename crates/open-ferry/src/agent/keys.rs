//! `open-ferry keys`: the client keys in `access.api-keys`, which clients
//! send to use the proxy: listing them masked, adding one (made here, or
//! read from standard input or a file), and removing one.
//!
//! A key made here is shown once: on the command line, or by the tool
//! with `confirm: true`; else the tool writes it to a new file the call
//! names, which on Unix only the user can read. A listed key is shown in
//! full only on the command line with `--reveal --yes`; no tool shows one.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use open_ferry_core::config::save;
use open_ferry_core::config::v8_edit::V8Method;
use open_ferry_dashboard::mask_client_key;
use serde::Serialize;
use serde_json::{Value, json};

use super::change::{
    Call, Content, Edit, Request, check_expected, key_text, make_from, read_config,
};
use super::config::{Source, reads_a_file, secret_in_argument};
use super::guard::confirm;
use super::values::{get, read_tree, tree_of};
use super::{Caller, Context, Failure, Outcome, Report};

/// Where the client keys are.
const PATH: [&str; 2] = ["access", "api-keys"];

/// `keys list`'s input.
#[derive(Clone, Debug, Default)]
pub(crate) struct ListInput {
    /// Show the keys in full: the command line only, with `--yes`.
    pub(crate) reveal: bool,
}

/// `keys add`'s input.
#[derive(Clone, Debug, Default)]
pub(crate) struct AddInput {
    /// Make a new key.
    pub(crate) generate: bool,
    /// Where the key to add comes from, when it isn't made here.
    pub(crate) source: Option<Source>,
    /// A new file to write a key made here to, instead of showing it.
    pub(crate) to_file: Option<PathBuf>,
}

/// `keys remove`'s input.
#[derive(Clone, Debug, Default)]
pub(crate) struct RemoveInput {
    /// The key's place in the list, from 0.
    pub(crate) index: Option<usize>,
    /// Where the key to remove comes from, when it isn't named by place.
    pub(crate) source: Option<Source>,
}

fn path() -> Vec<String> {
    PATH.iter().map(|part| (*part).to_owned()).collect()
}

/// The client keys in the config file.
fn current(ctx: &Context) -> Result<Vec<String>, Failure> {
    Ok(keys_of(&read_tree(&ctx.path)?))
}

/// The client keys in the config whose settings are `tree`.
fn keys_of(tree: &Value) -> Vec<String> {
    get(tree, &path())
        .and_then(Value::as_array)
        .map(|keys| keys.iter().map(key_text).collect())
        .unwrap_or_default()
}

/// A listed key.
#[derive(Debug, Serialize)]
struct Listed {
    index: usize,
    key: String,
}

/// The client keys.
#[derive(Debug, Serialize)]
struct Keys {
    count: usize,
    keys: Vec<Listed>,
    revealed: bool,
}

impl Report for Keys {
    fn text(&self) -> String {
        if self.keys.is_empty() {
            return "No client keys: access.api-keys is empty, so the proxy refuses every client (`open-ferry keys add --generate` makes one).\n".to_owned();
        }
        let mut out = format!(
            "{} client key{} in access.api-keys{}:\n",
            self.count,
            if self.count == 1 { "" } else { "s" },
            if self.revealed {
                ", in full"
            } else {
                ", masked"
            }
        );
        for listed in &self.keys {
            out.push_str(&format!("  {}  {}\n", listed.index, listed.key));
        }
        out
    }
}

/// `keys list`.
pub(crate) fn list(ctx: &Context, input: &ListInput) -> Result<Outcome, Failure> {
    let keys = current(ctx)?;
    if input.reveal {
        if ctx.caller == Caller::Mcp {
            return Err(Failure::usage("no tool shows a client key in full")
                .hint("run `open-ferry keys list --reveal --yes` in a terminal to see them"));
        }
        confirm(
            ctx,
            "Showing the client keys in full",
            &["it prints secrets".to_owned()],
            json!({}),
        )?;
    }
    let report = Keys {
        count: keys.len(),
        keys: keys
            .iter()
            .enumerate()
            .map(|(index, key)| Listed {
                index,
                key: if input.reveal {
                    key.clone()
                } else {
                    mask_client_key(key)
                },
            })
            .collect(),
        revealed: input.reveal,
    };
    let mut outcome = Outcome::of(&report);
    if input.reveal {
        for key in keys {
            outcome = outcome.reveal(key);
        }
    }
    Ok(outcome)
}

/// Checks `key` can be a client key.
fn check_key(key: &str) -> Result<(), Failure> {
    if key.is_empty() {
        return Err(Failure::new("invalid_value", "the key is empty"));
    }
    if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Failure::new(
            "invalid_value",
            "a client key can't hold spaces or control characters",
        ));
    }
    Ok(())
}

/// Writes `key` to the new file `path`, which on Unix only the user can
/// read.
fn write_key_file(path: &Path, key: &str) -> Result<(), Failure> {
    let failed = |error: std::io::Error| {
        Failure::new(
            "failed",
            format!("can't write the key to {}: {error}", path.display()),
        )
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(failed)?;
    let written = file
        .write_all(format!("{key}\n").as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(error) = written {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(failed(error));
    }
    Ok(())
}

/// `keys add`.
pub(crate) async fn add(ctx: &Context, input: AddInput) -> Result<Outcome, Failure> {
    let key = match (input.generate, &input.source) {
        (true, None) => {
            let bytes =
                crate::init::random_bytes().map_err(|error| Failure::new("failed", error))?;
            crate::init::client_key(&bytes)
        }
        (false, Some(source)) if source.is_inline() => {
            return Err(secret_in_argument(ctx, "a client key"));
        }
        (false, Some(source)) => source.text(ctx)?.trim().to_owned(),
        _ => {
            return Err(Failure::usage(match ctx.caller {
                Caller::Cli => "give one of --generate, --from-stdin and --from-file",
                Caller::Mcp => "give one of generate: true and from_file",
            }));
        }
    };
    check_key(&key)?;
    if input.to_file.is_some() && !input.generate {
        return Err(Failure::usage(
            "only a key made here is written to a file; this one is in a file already",
        ));
    }
    // The change is worked out from these bytes, and the list from the
    // file as it is each time it is worked out, so a key added or removed
    // since isn't undone.
    let data = read_config(&ctx.path)?;
    check_expected(ctx, &data)?;
    let show = input.generate && input.to_file.is_none();
    if show && ctx.caller == Caller::Mcp && !ctx.yes {
        let sha256 = save::sha256_hex(&data);
        return Err(Failure::new(
            "needs_confirmation",
            "keys_add returns a new key only with confirm: true, as the key then sits in the transcript; nothing was changed",
        )
        .hint(format!("call it again with confirm: true and expect_sha256: \"{sha256}\", or with to_file naming a new file to write the key to"))
        .would(json!({"config_sha256": sha256})));
    }
    if keys_of(&tree_of(&data)?).contains(&key) {
        return Err(Failure::new(
            "exists",
            "that key is in access.api-keys already; nothing was changed",
        ));
    }
    if let Some(file) = &input.to_file {
        write_key_file(file, &key)?;
    }
    let result = make_from(
        ctx,
        Request {
            edit: Edit {
                method: V8Method::Put,
                parts: path(),
                content: Content::KeyAdded(key.clone()),
            },
            call: Call::AddKey(key.clone()),
            action: "add_key",
            what: "Adding a client key".to_owned(),
            path: Some("access.api-keys".to_owned()),
            always: input
                .source
                .as_ref()
                .map(|source| reads_a_file(ctx, source))
                .unwrap_or_default(),
            hidden: input.source.is_some(),
        },
        data,
    )
    .await;
    let mut changed = match result {
        Ok(changed) => changed,
        Err(failure) => {
            if let Some(file) = &input.to_file {
                let _ = std::fs::remove_file(file);
            }
            return Err(failure);
        }
    };
    // Where it landed in the file as it is now; another write since can
    // have moved it, or the file can be unreadable, and then it is left out.
    if let Some(index) = current(ctx)
        .ok()
        .and_then(|keys| keys.iter().position(|listed| *listed == key))
    {
        changed.extra.insert("index".to_owned(), json!(index));
    }
    changed
        .extra
        .insert("masked".to_owned(), json!(mask_client_key(&key)));
    let mut outcome;
    if show {
        changed.extra.insert("key".to_owned(), json!(key));
        changed.footer = Some(format!(
            "The new client key, shown this once, so keep it now:\n  {key}"
        ));
        outcome = Outcome::of(&changed);
        outcome = outcome.reveal(key);
    } else if let Some(file) = &input.to_file {
        changed
            .extra
            .insert("key_file".to_owned(), json!(file.display().to_string()));
        changed.footer = Some(format!("The new client key is in {}.", file.display()));
        outcome = Outcome::of(&changed);
    } else {
        outcome = Outcome::of(&changed);
    }
    Ok(outcome)
}

/// `keys remove`.
pub(crate) async fn remove(ctx: &Context, input: RemoveInput) -> Result<Outcome, Failure> {
    // As for `add`: the key is found in these bytes, and removed by its
    // value from the list the file holds each time it is worked out.
    let data = read_config(&ctx.path)?;
    check_expected(ctx, &data)?;
    let keys = keys_of(&tree_of(&data)?);
    let index = match (input.index, &input.source) {
        (Some(index), None) => {
            if index >= keys.len() {
                return Err(Failure::new(
                    "not_found",
                    format!(
                        "there is no client key {index}: access.api-keys has {}",
                        keys.len()
                    ),
                )
                .hint(list_hint(ctx)));
            }
            index
        }
        (None, Some(source)) if source.is_inline() => {
            return Err(secret_in_argument(ctx, "a client key"));
        }
        (None, Some(source)) => {
            let key = source.text(ctx)?.trim().to_owned();
            keys.iter()
                .position(|listed| *listed == key)
                .ok_or_else(|| {
                    Failure::new("not_found", "that key isn't in access.api-keys")
                        .hint(list_hint(ctx))
                })?
        }
        _ => {
            return Err(Failure::usage(match ctx.caller {
                Caller::Cli => {
                    "name the key by its index, or give it with --from-stdin or --from-file"
                }
                Caller::Mcp => "name the key by its index, or give it with from_file",
            }));
        }
    };
    let key = keys.get(index).cloned().unwrap_or_default();
    let masked = mask_client_key(&key);
    let mut changed = make_from(
        ctx,
        Request {
            edit: Edit {
                method: V8Method::Put,
                parts: path(),
                content: Content::KeyRemoved(key.clone()),
            },
            call: Call::RemoveKey(key),
            action: "remove_key",
            what: format!("Removing client key {index} ({masked})"),
            path: Some("access.api-keys".to_owned()),
            always: vec![format!(
                "it deletes client key {index} ({masked}), and a client using it is refused"
            )],
            hidden: input.source.is_some(),
        },
        data,
    )
    .await?;
    changed.extra.insert("index".to_owned(), json!(index));
    changed.extra.insert("masked".to_owned(), json!(masked));
    Ok(Outcome::of(&changed))
}

fn list_hint(ctx: &Context) -> String {
    format!("list them with `{}`", ctx.command_name("keys list"))
}
