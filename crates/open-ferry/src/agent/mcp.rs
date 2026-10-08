//! `open-ferry mcp`: the commands of this module as tools of a Model
//! Context Protocol server on standard input and output, for agent apps
//! such as Claude Code, Codex, T3 Code and Cursor (see `docs/mcp.md`).
//!
//! Each tool is one command: `config set` is `config_set`, with the same
//! inputs and the same JSON, which is the tool's structured result; its
//! text is the command's text. A failure is a tool error with the
//! command's failure JSON (`error`, `message`, `hint`, `would`). A change
//! that needs `--yes` on the command line needs `confirm: true`; without
//! it, nothing is changed and the failure says what would be. No tool
//! returns a secret that is already in the setup: no tool lists the client
//! keys in full, and `clients_setup` masks the key it puts in a setup. A
//! key `keys_add` makes is returned only with `confirm: true`, else written
//! to a new file the call names.
//!
//! The resources are `docs/agents.md` and the config, masked.
//!
//! Standard output carries the protocol only: nothing is logged, and a
//! failure to start goes to standard error.
//!
//! Upstream has no MCP server.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt as _};
use serde_json::{Value, json};

use super::clients::{CLIENTS, SetupInput, Shell};
use super::config::{GetInput, ReplaceInput, SetInput, Source, UnsetInput};
use super::credentials::{ListInput as CredentialsList, LoginInput, STATES, TargetInput};
use super::keys::{AddInput, ListInput as KeysList, RemoveInput};
use super::target::Env;
use super::{Caller, Command, Context, Failure, Outcome, exit, perform};

/// The URI of `docs/agents.md`.
pub(crate) const DOCS_URI: &str = "open-ferry://docs/agents.md";

/// The URI of the config, masked.
pub(crate) const CONFIG_URI: &str = "open-ferry://config";

/// `docs/agents.md`.
const AGENTS_DOC: &str = include_str!("../../../../docs/agents.md");

/// What the server tells a client about itself.
const INSTRUCTIONS: &str = "Tools to look at and change this machine's open-ferry setup: \
open-ferry is a local proxy that lets apps use the user's AI subscriptions and API keys. \
Start with `status`. Settings are paths in the v8 config, as `routing.strategy`; \
`config_get` says whether one is set and its default. A change that deletes, reveals, \
replaces the whole config or touches a sensitive setting needs `confirm: true`: without it \
nothing is changed and the result says what would be, so show that to the user and ask \
before calling again with `confirm: true`, and with that result's `config_sha256` as \
`expect_sha256`, so the change is made only to the config the user saw. Every change can be reversed with `config_undo`. \
No tool returns a secret already in the setup; never ask the user to paste one into the \
conversation, but to put it in a file and give its path as `from_file`; a file is read only \
for a secret, never from the auth directory, and never a credential file (one with a PEM \
block, or with a sign-in's tokens or a key at any depth), and a call with `from_file` needs \
`confirm: true`. \
The resource open-ferry://docs/agents.md has the details.";

/// What makes a change of the settings need `confirm: true`, for the tool
/// descriptions; it names each of `guard::SENSITIVE_SETTINGS` (a test
/// checks it).
macro_rules! sensitive {
    () => {
        "A change to a sensitive setting needs `confirm: true`: management.allow-remote, management.secret-key or management.separate-address; server.host unset or set to an address that isn't loopback; anything under server.tls or server.trusted-proxies; or removing the last client key in access.api-keys (blank and repeated keys don't count)."
    };
}

/// A tool: its command, and how it is described to a client.
struct Spec {
    /// The tool's name.
    name: &'static str,
    /// The command it runs, as the command line names it.
    command: &'static str,
    title: &'static str,
    description: &'static str,
    read_only: bool,
    destructive: bool,
    idempotent: bool,
    open_world: bool,
    /// The input's properties.
    properties: fn() -> Value,
    /// The properties a call must give.
    required: &'static [&'static str],
}

/// The tools, one per command.
const TOOLS: [Spec; 18] = [
    Spec {
        name: "status",
        command: "status",
        title: "open-ferry status",
        description: "Whether an open-ferry server runs for the config, at which address and version, how many credentials it has in each state, today's calls and errors, and how many client keys the config has. `running` is false when no server runs; the settings tools then change the config file, and the tools that need the server say so.",
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
        properties: no_properties,
        required: &[],
    },
    Spec {
        name: "config_get",
        command: "config get",
        title: "Get a setting",
        description: "One setting of the config, by its path in the v8 config (dotted, as `routing.strategy` or `server.port`). When it isn't set, `set` is false and `default` is the value the server uses. Secrets are masked. An unknown path is refused with the nearest known one.",
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
        properties: path_properties,
        required: &["path"],
    },
    Spec {
        name: "config_set",
        command: "config set",
        title: "Change a setting",
        description: concat!(
            "Sets one setting of the config to `value` (any JSON). With a running server the change goes through it and applies at once; else it is written to the config file. A secret (an API key, a password, or a list that holds them) is refused as `value`: put it in a file and give `from_file`, which is only for a secret (a setting that holds none is refused from a file), needs `confirm: true` as it reads a file into the config, and is never read from the auth directory or from a credential file. ",
            sensitive!(),
            " The result has each changed setting's old and new value, masked; `config_undo` reverses it."
        ),
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: false,
        properties: set_properties,
        required: &["path"],
    },
    Spec {
        name: "config_unset",
        command: "config unset",
        title: "Remove a setting",
        description: concat!(
            "Removes one setting from the config, so the server uses its default (see `config_get`). ",
            sensitive!(),
            " `config_undo` reverses it."
        ),
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: false,
        properties: path_confirm_properties,
        required: &["path"],
    },
    Spec {
        name: "config_show",
        command: "config show",
        title: "Show the config",
        description: "The whole config, with every secret masked: `settings` is the config as JSON.",
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
        properties: no_properties,
        required: &[],
    },
    Spec {
        name: "config_diff",
        command: "config diff",
        title: "Show the last change",
        description: "What the last change to the config made: each setting that differs from the backup the change left (the config's .bak file), with old and new values, masked.",
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
        properties: no_properties,
        required: &[],
    },
    Spec {
        name: "config_undo",
        command: "config undo",
        title: "Undo the last change",
        description: concat!(
            "Reverses the last change to the config, by swapping the config and its backup; calling it again redoes the change. When the config was changed since the last backup, as by a hand edit, undoing loses that change too, so it fails with `changed_since` and lists what it would change unless `confirm: true`. ",
            sensitive!()
        ),
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: false,
        properties: confirm_properties,
        required: &[],
    },
    Spec {
        name: "config_replace",
        command: "config replace",
        title: "Replace the whole config",
        description: "Replaces the whole config with the YAML in the file `from_file`, after checking it. It always needs `confirm: true`, as it replaces the whole config and reads a file into it; without it the result lists every setting it would change. `config_undo` reverses it.",
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: false,
        properties: replace_properties,
        required: &["from_file"],
    },
    Spec {
        name: "keys_list",
        command: "keys list",
        title: "List the client keys",
        description: "The client keys in access.api-keys, which apps send to use the proxy, masked, with their indexes. No tool shows them in full; the user can, with `open-ferry keys list --reveal --yes` in a terminal.",
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
        properties: no_properties,
        required: &[],
    },
    Spec {
        name: "keys_add",
        command: "keys add",
        title: "Add a client key",
        description: "Adds a client key to access.api-keys: with `generate: true` a new random key, else the key in the file `from_file`, which needs `confirm: true` as it reads a file into the config. A new key is written to the new file `to_file` (on Unix only the user can read it), or, with `confirm: true` and no `to_file`, returned once as `key`, which puts it in this conversation. Prefer `to_file`.",
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
        properties: add_properties,
        required: &[],
    },
    Spec {
        name: "keys_remove",
        command: "keys remove",
        title: "Remove a client key",
        description: "Removes a client key, by its `index` from `keys_list`, or the key in the file `from_file`. An app using it is refused afterwards, so it needs `confirm: true`. `config_undo` reverses it.",
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: false,
        properties: remove_properties,
        required: &[],
    },
    Spec {
        name: "credentials_list",
        command: "credentials list",
        title: "List the credentials",
        description: "The running server's credentials (subscription sign-ins and API keys), each with its `auth_index`, provider, account (masked) and `state`: ready, resting (cooling down, see `cooldown`), failing, refreshing, waiting, off (disabled) or unknown. Filter by `state` or `provider`. It needs the server.",
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
        properties: credentials_list_properties,
        required: &[],
    },
    Spec {
        name: "credentials_enable",
        command: "credentials enable",
        title: "Turn a credential on",
        description: "Turns a credential back on, so requests are routed to it again. `credential` is its `auth_index` or name from `credentials_list`. It needs the server.",
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: false,
        properties: credential_properties,
        required: &["credential"],
    },
    Spec {
        name: "credentials_disable",
        command: "credentials disable",
        title: "Turn a credential off",
        description: "Turns a credential off, so no request is routed to it until `credentials_enable`. `credential` is its `auth_index` or name from `credentials_list`. It needs the server.",
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: false,
        properties: credential_properties,
        required: &["credential"],
    },
    Spec {
        name: "credentials_reset_quota",
        command: "credentials reset-quota",
        title: "Clear a credential's cooldown",
        description: "Clears a credential's cooldowns and quota state, so it is tried again at once. `credential` is its `auth_index` or name from `credentials_list`. It needs the server.",
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: false,
        properties: credential_properties,
        required: &["credential"],
    },
    Spec {
        name: "credentials_remove",
        command: "credentials remove",
        title: "Delete a credential",
        description: "Deletes a credential's file, so the sign-in is gone and must be done again to use it. It needs `confirm: true` and the server; `config_undo` doesn't bring it back.",
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: false,
        properties: credential_confirm_properties,
        required: &["credential"],
    },
    Spec {
        name: "credentials_login",
        command: "credentials login",
        title: "Sign in to a provider",
        description: "Starts a sign-in through the running server: `provider` is codex (ChatGPT) or claude. It returns a `url` for the user to open in a browser and a `state`, with `status` \"wait\". Show the user the URL, then call again with the same `provider` and `state` to wait for the sign-in to finish: `status` becomes \"ok\" or \"error\", or stays \"wait\" when it is still going: not an error, so call again with the same `state`. A Claude sign-in needs `confirm: true`, as Anthropic's terms may not allow it (see docs/claude-subscription.md).",
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: true,
        properties: login_properties,
        required: &["provider"],
    },
    Spec {
        name: "clients_setup",
        command: "clients setup",
        title: "Show an app's setup",
        description: "The settings and a sample request for using the proxy from `client`: the base URL, a model it serves and the client key, masked, to replace with the user's key. It changes nothing.",
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
        properties: setup_properties,
        required: &["client"],
    },
];

fn confirm_property() -> Value {
    json!({
        "type": "boolean",
        "description": "Go ahead with a change that needs a confirmation. Without it such a change is not made, and the result says what it would change: show that to the user and ask first."
    })
}

fn expect_sha256_property() -> Value {
    json!({
        "type": "string",
        "description": "The `config_sha256` a result that needed `confirm: true` gave: send it with `confirm: true`, so the change is made only if the config is still the one that result was worked out from. Else it fails with `config_changed`, and nothing is changed."
    })
}

fn path_property() -> Value {
    json!({
        "type": "string",
        "description": "The setting's path in the v8 config, dotted, as `routing.strategy` or `server.port`. A list is set whole, never one item by its index. Use `/` between keys when a key holds a dot."
    })
}

fn from_file_property(what: &str) -> Value {
    json!({
        "type": "string",
        "description": format!("The path of a file that holds {what}. A secret must come this way, never in the call. A file in the auth directory, or a credential file (one with a PEM block, or with a sign-in's tokens or a key, such as `access_token`, `accessToken`, `tokens` or `private_key`, at any depth), is refused. A call with it needs `confirm: true`, as it reads a file into the config.")
    })
}

fn no_properties() -> Value {
    json!({})
}

fn confirm_properties() -> Value {
    json!({ "confirm": confirm_property(), "expect_sha256": expect_sha256_property() })
}

fn path_properties() -> Value {
    json!({ "path": path_property() })
}

fn path_confirm_properties() -> Value {
    json!({
        "path": path_property(),
        "confirm": confirm_property(),
        "expect_sha256": expect_sha256_property(),
    })
}

fn set_properties() -> Value {
    json!({
        "path": path_property(),
        "value": {
            "description": "The new value, as JSON: a string, number, boolean, list or object. Not a secret: give that with `from_file`."
        },
        "from_file": from_file_property("the new value when it is a secret, as YAML or JSON, or as text with `string`"),
        "string": {
            "type": "boolean",
            "description": "Take the value as a string, as it is, rather than as YAML."
        },
        "confirm": confirm_property(),
        "expect_sha256": expect_sha256_property(),
    })
}

fn replace_properties() -> Value {
    json!({
        "from_file": from_file_property("the new config, as YAML"),
        "confirm": confirm_property(),
        "expect_sha256": expect_sha256_property(),
    })
}

fn add_properties() -> Value {
    json!({
        "generate": {
            "type": "boolean",
            "description": "Make a new random key."
        },
        "from_file": from_file_property("the key to add"),
        "to_file": {
            "type": "string",
            "description": "With `generate`: the path of a new file to write the key to, instead of returning it. The file mustn't exist."
        },
        "confirm": confirm_property(),
        "expect_sha256": expect_sha256_property(),
    })
}

fn remove_properties() -> Value {
    json!({
        "index": {
            "type": "integer",
            "minimum": 0,
            "description": "The key's index, from `keys_list`."
        },
        "from_file": from_file_property("the key to remove"),
        "confirm": confirm_property(),
        "expect_sha256": expect_sha256_property(),
    })
}

fn credentials_list_properties() -> Value {
    json!({
        "state": {
            "type": "string",
            "enum": STATES,
            "description": "Only the credentials in this state."
        },
        "provider": {
            "type": "string",
            "description": "Only this provider's credentials, as `codex`, `claude` or `gemini`."
        },
    })
}

fn credential_property() -> Value {
    json!({
        "type": "string",
        "description": "The credential's `auth_index`, or its name, from `credentials_list`."
    })
}

fn credential_properties() -> Value {
    json!({ "credential": credential_property() })
}

fn credential_confirm_properties() -> Value {
    json!({ "credential": credential_property(), "confirm": confirm_property() })
}

fn login_properties() -> Value {
    json!({
        "provider": {
            "type": "string",
            "enum": ["codex", "claude"],
            "description": "codex for a ChatGPT sign-in, claude for a Claude one."
        },
        "state": {
            "type": "string",
            "description": "The `state` a first call returned: wait for that sign-in to finish."
        },
        "confirm": confirm_property(),
    })
}

fn setup_properties() -> Value {
    json!({
        "client": {
            "type": "string",
            "enum": CLIENTS,
            "description": "The app or library to set up."
        },
        "model": {
            "type": "string",
            "description": "The model to put in the sample; else one the server serves that suits the client."
        },
        "shell": {
            "type": "string",
            "enum": ["posix", "powershell"],
            "description": "The shell the commands are for; else this machine's."
        },
        "key_index": {
            "type": "integer",
            "minimum": 0,
            "description": "Which client key to put in, masked, by its index from `keys_list`; else the first."
        },
    })
}

impl Spec {
    /// The tool, as listed.
    fn tool(&self) -> Tool {
        let mut schema = JsonObject::new();
        schema.insert("type".to_owned(), json!("object"));
        schema.insert("properties".to_owned(), (self.properties)());
        if !self.required.is_empty() {
            schema.insert("required".to_owned(), json!(self.required));
        }
        schema.insert("additionalProperties".to_owned(), json!(false));
        let mut annotations = ToolAnnotations::new()
            .read_only(self.read_only)
            .idempotent(self.idempotent)
            .open_world(self.open_world);
        if !self.read_only {
            annotations = annotations.destructive(self.destructive);
        }
        let description = format!(
            "{} The command line's `open-ferry {}` does the same.",
            self.description, self.command
        );
        Tool::new(self.name, description, Arc::new(schema))
            .with_title(self.title)
            .with_annotations(annotations)
    }
}

/// A tool call's arguments.
struct Arguments {
    map: JsonObject,
}

impl Arguments {
    /// `map`, checked against what `spec` takes.
    fn new(spec: &Spec, map: Option<JsonObject>) -> Result<Self, Failure> {
        let map = map.unwrap_or_default();
        let properties = (spec.properties)();
        for name in map.keys() {
            if properties.get(name).is_none() {
                return Err(
                    Failure::usage(format!("{} takes no argument {name:?}", spec.name))
                        .hint(takes(spec)),
                );
            }
        }
        for name in spec.required {
            if map.get(*name).is_none_or(Value::is_null) {
                return Err(
                    Failure::usage(format!("{} needs the argument {name:?}", spec.name))
                        .hint(takes(spec)),
                );
            }
        }
        Ok(Self { map })
    }

    fn value(&self, name: &str) -> Option<&Value> {
        self.map.get(name).filter(|value| !value.is_null())
    }

    fn string(&self, name: &str) -> Result<Option<String>, Failure> {
        match self.value(name) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(Failure::usage(format!("{name} must be a string"))),
        }
    }

    fn flag(&self, name: &str) -> Result<bool, Failure> {
        match self.value(name) {
            None => Ok(false),
            Some(Value::Bool(flag)) => Ok(*flag),
            Some(_) => Err(Failure::usage(format!("{name} must be true or false"))),
        }
    }

    fn index(&self, name: &str) -> Result<Option<usize>, Failure> {
        match self.value(name) {
            None => Ok(None),
            Some(value) => value
                .as_u64()
                .and_then(|index| usize::try_from(index).ok())
                .map(Some)
                .ok_or_else(|| Failure::usage(format!("{name} must be a whole number, from 0"))),
        }
    }

    /// `from_file`, as a source.
    fn file(&self) -> Result<Option<Source>, Failure> {
        Ok(self
            .string("from_file")?
            .map(|file| Source::File(PathBuf::from(file))))
    }
}

/// What `spec` takes, for a hint.
fn takes(spec: &Spec) -> String {
    let properties = (spec.properties)();
    let names: Vec<&str> = properties
        .as_object()
        .map(|properties| properties.keys().map(String::as_str).collect())
        .unwrap_or_default();
    if names.is_empty() {
        format!("{} takes no arguments", spec.name)
    } else {
        format!("{} takes: {}", spec.name, names.join(", "))
    }
}

/// The command `spec` runs, with `args`.
fn command(spec: &Spec, args: &Arguments) -> Result<Command, Failure> {
    let credential = || -> Result<TargetInput, Failure> {
        Ok(TargetInput {
            credential: args.string("credential")?.unwrap_or_default(),
        })
    };
    let path = || args.string("path").map(Option::unwrap_or_default);
    Ok(match spec.name {
        "status" => Command::Status,
        "config_get" => Command::ConfigGet(GetInput { path: path()? }),
        "config_set" => {
            let value = match (args.value("value"), args.file()?) {
                (Some(value), None) => Source::Json(value.clone()),
                (None, Some(file)) => file,
                _ => {
                    return Err(Failure::usage(
                        "give the value as `value` or `from_file`, one of them",
                    ));
                }
            };
            Command::ConfigSet(SetInput {
                path: path()?,
                value,
                string: args.flag("string")?,
            })
        }
        "config_unset" => Command::ConfigUnset(UnsetInput { path: path()? }),
        "config_show" => Command::ConfigShow,
        "config_diff" => Command::ConfigDiff,
        "config_undo" => Command::ConfigUndo,
        "config_replace" => Command::ConfigReplace(ReplaceInput {
            source: args
                .file()?
                .ok_or_else(|| Failure::usage("give the new config's file as `from_file`"))?,
        }),
        "keys_list" => Command::KeysList(KeysList { reveal: false }),
        "keys_add" => Command::KeysAdd(AddInput {
            generate: args.flag("generate")?,
            source: args.file()?,
            to_file: args.string("to_file")?.map(PathBuf::from),
        }),
        "keys_remove" => {
            let index = args.index("index")?;
            let source = args.file()?;
            if index.is_some() && source.is_some() {
                return Err(Failure::usage(
                    "name the key by `index` or `from_file`, not both",
                ));
            }
            Command::KeysRemove(RemoveInput { index, source })
        }
        "credentials_list" => Command::CredentialsList(CredentialsList {
            state: args.string("state")?,
            provider: args.string("provider")?,
        }),
        "credentials_enable" => Command::CredentialsEnable(credential()?),
        "credentials_disable" => Command::CredentialsDisable(credential()?),
        "credentials_reset_quota" => Command::CredentialsResetQuota(credential()?),
        "credentials_remove" => Command::CredentialsRemove(credential()?),
        "credentials_login" => Command::CredentialsLogin(LoginInput {
            provider: args.string("provider")?.unwrap_or_default(),
            state: args.string("state")?,
            wait: false,
        }),
        "clients_setup" => Command::ClientsSetup(SetupInput {
            client: args.string("client")?.unwrap_or_default(),
            model: args.string("model")?,
            shell: args
                .string("shell")?
                .map(|shell| Shell::parse(&shell))
                .transpose()?,
            key_index: args.index("key_index")?,
            reveal: false,
        }),
        other => return Err(Failure::usage(format!("no tool is named {other}"))),
    })
}

/// The tool named `name`.
fn spec(name: &str) -> Option<&'static Spec> {
    TOOLS.iter().find(|spec| spec.name == name)
}

/// The server: the config it was started for, and the environment's key.
#[derive(Clone)]
pub(crate) struct Server {
    path: Result<PathBuf, Failure>,
    env: Env,
    key_file: Option<PathBuf>,
}

impl Server {
    pub(crate) fn new(path: Result<PathBuf, Failure>, env: Env, key_file: Option<PathBuf>) -> Self {
        Self {
            path,
            env,
            key_file,
        }
    }

    /// The context of a tool call, confirmed with `yes`, for the config
    /// whose SHA-256 is `expect_sha256` when it is given.
    fn context(&self, yes: bool, expect_sha256: Option<String>) -> Result<Context, Failure> {
        Ok(Context {
            path: self.path.clone()?,
            env: self.env.clone(),
            key_file: self.key_file.clone(),
            yes,
            expect_sha256,
            ask: None,
            say: None,
            caller: Caller::Mcp,
        })
    }

    /// Runs the tool `name` with `arguments`.
    pub(crate) async fn call(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<Outcome, Failure> {
        let spec = spec(name).ok_or_else(|| {
            Failure::usage(format!("no tool is named {name}"))
                .hint("the tools/list request lists them")
        })?;
        let args = Arguments::new(spec, arguments)?;
        let yes = args.flag("confirm")?;
        let expect_sha256 = args
            .string("expect_sha256")?
            .map(|text| super::change::parse_sha256(&text, "expect_sha256"))
            .transpose()?;
        let command = command(spec, &args)?;
        let ctx = self.context(yes, expect_sha256)?;
        perform(&ctx, command).await
    }

    /// The config, masked, as YAML.
    async fn masked_config(&self) -> Result<String, Failure> {
        let ctx = self.context(false, None)?;
        perform(&ctx, Command::ConfigShow)
            .await
            .map(|outcome| outcome.text)
    }
}

/// `outcome`, or `failure`, as a tool's result: its JSON as the structured
/// result and the first text item, then its text.
pub(crate) fn tool_result(result: Result<Outcome, Failure>) -> CallToolResult {
    match result {
        Ok(outcome) => {
            let mut result = if outcome.code == exit::FAILED {
                CallToolResult::structured_error(outcome.json)
            } else {
                CallToolResult::structured(outcome.json)
            };
            result.content.push(ContentBlock::text(outcome.text));
            result
        }
        Err(failure) => {
            let text = failure.text();
            let mut result = CallToolResult::structured_error(
                serde_json::to_value(&failure).unwrap_or(Value::Null),
            );
            result.content.push(ContentBlock::text(text));
            result
        }
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_instructions(INSTRUCTIONS)
        .with_server_info(
            Implementation::new("open-ferry", env!("CARGO_PKG_VERSION")).with_title("open-ferry"),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            TOOLS.iter().map(Spec::tool).collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if spec(&request.name).is_none() {
            return Err(ErrorData::invalid_params(
                format!("no tool is named {}", request.name),
                None,
            ));
        }
        Ok(tool_result(self.call(&request.name, request.arguments).await).into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new(DOCS_URI, "agents.md")
                .with_title("open-ferry for agents")
                .with_description("How an agent looks at and changes an open-ferry setup: the commands, the tools, the guardrails and the management API.")
                .with_mime_type("text/markdown"),
            Resource::new(CONFIG_URI, "config")
                .with_title("The config, masked")
                .with_description("The open-ferry config these tools work on, as YAML, with every secret masked.")
                .with_mime_type("application/yaml"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let (text, mime) = match request.uri.as_str() {
            DOCS_URI => (AGENTS_DOC.to_owned(), "text/markdown"),
            CONFIG_URI => match self.masked_config().await {
                Ok(text) => (text, "application/yaml"),
                Err(failure) => {
                    return Err(ErrorData::resource_not_found(
                        failure.message,
                        failure.hint.map(|hint| json!({ "hint": hint })),
                    ));
                }
            },
            other => {
                return Err(ErrorData::resource_not_found(
                    format!("no resource is at {other}"),
                    None,
                ));
            }
        };
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, request.uri).with_mime_type(mime),
        ])
        .into())
    }
}

/// Serves the tools on standard input and output until the client closes
/// them.
pub(crate) fn main(
    path: Result<PathBuf, Failure>,
    env: Env,
    key_file: Option<PathBuf>,
) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("can't start the async runtime: {error}");
            return ExitCode::from(exit::FAILED);
        }
    };
    let server = Server::new(path, env, key_file);
    runtime.block_on(async move {
        let running = match server.serve(rmcp::transport::stdio()).await {
            Ok(running) => running,
            Err(error) => {
                eprintln!("the MCP session didn't start: {error}");
                return ExitCode::from(exit::FAILED);
            }
        };
        match running.waiting().await {
            Ok(_) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("the MCP session failed: {error}");
                ExitCode::from(exit::FAILED)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: every tool is a command of the command line, with
    // the same name, and its schema lists the arguments it takes.
    #[test]
    fn names_tools_as_commands() {
        for spec in &TOOLS {
            assert_eq!(spec.name, spec.command.replace([' ', '-'], "_"));
            let tool = spec.tool();
            assert_eq!(
                tool.input_schema.get("additionalProperties"),
                Some(&json!(false))
            );
            for required in spec.required {
                assert!(
                    (spec.properties)().get(*required).is_some(),
                    "{}: {required}",
                    spec.name
                );
            }
        }
        let names: std::collections::BTreeSet<&str> = TOOLS.iter().map(|spec| spec.name).collect();
        assert_eq!(names.len(), TOOLS.len());
    }

    // Not upstream's: the tools that change settings name every setting a
    // change to which needs `confirm: true`, as `guard.rs` judges them.
    #[test]
    fn descriptions_name_the_sensitive_settings() {
        for name in ["config_set", "config_unset", "config_undo"] {
            let spec = TOOLS.iter().find(|spec| spec.name == name).unwrap();
            for (setting, _) in super::super::guard::SENSITIVE_SETTINGS {
                assert!(spec.description.contains(setting), "{name}: {setting}");
            }
            assert!(spec.description.contains("server.host unset"), "{name}");
            assert!(
                spec.description.contains("removing the last client key"),
                "{name}"
            );
        }
    }

    // Not upstream's: an argument a tool doesn't take, or of the wrong
    // type, is refused before anything runs.
    #[test]
    fn checks_arguments() {
        let Some(get) = spec("config_get") else {
            panic!("no config_get");
        };
        let args = |value: Value| value.as_object().cloned();
        assert!(Arguments::new(get, args(json!({"path": "server.port"}))).is_ok());
        assert_eq!(
            Arguments::new(get, args(json!({"path": "a", "secret": "x"})))
                .err()
                .map(|failure| failure.error),
            Some("usage")
        );
        assert!(Arguments::new(get, None).is_err());
        let Some(remove) = spec("keys_remove") else {
            panic!("no keys_remove");
        };
        let Ok(parsed) = Arguments::new(remove, args(json!({"index": -1}))) else {
            panic!("didn't parse");
        };
        assert!(command(remove, &parsed).is_err());
    }
}
