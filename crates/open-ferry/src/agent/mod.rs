//! Commands to look at and change a setup, for people and for agents:
//! `status`, `config`, `keys`, `credentials` and `clients`, and `mcp`,
//! which serves the same commands as tools over the Model Context
//! Protocol (see [`mcp`]).
//!
//! Each command is an action here, with a typed input and a typed report;
//! the command line ([`cli`]) and the MCP server ([`mcp`]) only read the
//! input and print the report, so a tool and its command take the same
//! inputs and give the same JSON.
//!
//! An action finds the config as `check` does (`--config`, else
//! `config.yaml` in the working directory), and else the installed config
//! (see [`target`]). A server running for that config is reached through
//! its management API, at the address the config gives it, on loopback
//! only; the management key is the config's plain `management.secret-key`,
//! else `MANAGEMENT_PASSWORD`, else the key file `--management-key-file` or
//! `OPEN_FERRY_MANAGEMENT_KEY_FILE` names, and never one from the command
//! line. A setting changed while no server runs, or while the running one
//! can't be reached with a key, is written to the file with the writer the
//! server uses, which the server's file watcher picks up; a command that
//! needs the running server says that it isn't running, and how to start
//! it.
//!
//! What a command prints never holds a secret it wasn't asked to show:
//! secrets are masked as the dashboard masks client keys, and the output
//! is scrubbed of every secret of the config, `MANAGEMENT_PASSWORD` and the
//! key file before it is printed (see [`mask`]). A change that deletes,
//! reveals, replaces the config or touches a sensitive setting needs
//! `--yes` (`confirm: true` for a tool); without it, and without a
//! terminal to ask on, nothing is changed and the answer says what would
//! be (see [`guard`]). Every change shows each setting's old and new value,
//! masked, and that `open-ferry config undo` reverses it.
//!
//! Upstream has none of these commands.

mod api;
mod change;
mod cli;
mod clients;
mod config;
mod credentials;
mod guard;
mod keys;
mod mask;
mod mcp;
mod status;
mod target;
mod values;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::process::ExitCode;

use serde::Serialize;
use serde_json::Value;

pub(crate) use target::Env;

/// The commands this module runs, as the first argument.
const COMMANDS: [&str; 6] = ["status", "config", "keys", "credentials", "clients", "mcp"];

/// Whether `name`, the first argument, is one of these commands.
pub fn is_command(name: &str) -> bool {
    COMMANDS.contains(&name)
}

/// Runs the command `args` names, `args` holding the command and what
/// follows it. `main` calls it before any thread starts, as `.env` is
/// loaded here.
pub fn main<I>(program: &str, args: I) -> ExitCode
where
    I: IntoIterator<Item = String>,
{
    cli::main(program, args.into_iter().collect())
}

/// The exit codes.
pub(crate) mod exit {
    /// Done.
    pub(crate) const OK: u8 = 0;
    /// It failed, or was refused; nothing was changed unless the report
    /// says so.
    pub(crate) const FAILED: u8 = 1;
    /// Bad usage: an unknown command, flag or setting, or a secret given
    /// as an argument.
    pub(crate) const USAGE: u8 = 2;
    /// It needs a confirmation it didn't get; nothing was changed.
    pub(crate) const CONFIRM: u8 = 3;
    /// It needs the server, which isn't running (`status`: it isn't).
    pub(crate) const NOT_RUNNING: u8 = 4;
}

/// Who runs an action: it decides how a confirmation is asked for, and
/// whether an existing secret may be shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Caller {
    /// The command line.
    Cli,
    /// A tool call of the MCP server.
    Mcp,
}

/// Asks the person at the terminal a yes-or-no question.
pub(crate) type Ask = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// Tells the person at the terminal how a command is getting on.
pub(crate) type Say = Box<dyn Fn(&str) + Send + Sync>;

/// What an action runs with.
pub(crate) struct Context {
    /// The config file.
    pub(crate) path: PathBuf,
    /// The environment's management key and key file.
    pub(crate) env: Env,
    /// The key file `--management-key-file` names.
    pub(crate) key_file: Option<PathBuf>,
    /// `--yes`, or `confirm: true`.
    pub(crate) yes: bool,
    /// `--expect-sha256`, or `expect_sha256`: the SHA-256 the config file
    /// must have for a change to be made, as the `config_sha256` a result
    /// that needed a confirmation gave, in lowercase hex.
    pub(crate) expect_sha256: Option<String>,
    /// `--expect-backup-sha256`, or `expect_backup_sha256`, for an undo:
    /// the SHA-256 the backup must have for it to be put back, as the
    /// `backup_sha256` a result that needed a confirmation gave, in
    /// lowercase hex.
    pub(crate) expect_backup_sha256: Option<String>,
    /// How to ask for a confirmation, when there is a terminal to ask on.
    pub(crate) ask: Option<Ask>,
    /// How to tell how a command is getting on, while it runs.
    pub(crate) say: Option<Say>,
    /// Who runs it.
    pub(crate) caller: Caller,
}

impl Context {
    /// How the caller confirms: `--yes`, or `confirm: true`.
    pub(crate) fn confirm_flag(&self) -> &'static str {
        match self.caller {
            Caller::Cli => "--yes",
            Caller::Mcp => "confirm: true",
        }
    }

    /// How the caller names `command`: `config set` on the command line,
    /// `config_set` as a tool.
    pub(crate) fn command_name(&self, command: &str) -> String {
        match self.caller {
            Caller::Cli => format!("open-ferry {command}"),
            Caller::Mcp => command.replace([' ', '-'], "_"),
        }
    }
}

/// Why an action didn't do what it was asked. It changed nothing, unless
/// its message says otherwise.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Failure {
    /// What went wrong, as a code.
    pub(crate) error: &'static str,
    /// What went wrong, for people.
    pub(crate) message: String,
    /// What to do about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) hint: Option<String>,
    /// For a change that needs a confirmation: what it would change,
    /// masked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) would: Option<Value>,
    /// The exit code.
    #[serde(skip)]
    pub(crate) code: u8,
}

impl Failure {
    /// A failure with the code `error`, whose exit code follows from it.
    pub(crate) fn new(error: &'static str, message: impl Into<String>) -> Self {
        let code = match error {
            "usage" | "unknown_path" | "secret_in_argument" => exit::USAGE,
            "needs_confirmation" | "declined" => exit::CONFIRM,
            "not_running" => exit::NOT_RUNNING,
            _ => exit::FAILED,
        };
        Self {
            error,
            message: message.into(),
            hint: None,
            would: None,
            code,
        }
    }

    /// Bad usage.
    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self::new("usage", message)
    }

    /// With `hint`.
    pub(crate) fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// With `would`, what a change would make.
    pub(crate) fn would(mut self, would: Value) -> Self {
        self.would = Some(would);
        self
    }

    /// The failure as text: its message, then its hint and what it would
    /// change.
    pub(crate) fn text(&self) -> String {
        let mut out = format!("{}\n", self.message);
        if let Some(changes) = self
            .would
            .as_ref()
            .and_then(|would| would.get("changes"))
            .and_then(Value::as_array)
        {
            out.push_str("It would change:\n");
            for change in changes {
                out.push_str(&values::change_line(change));
            }
        }
        if let Some(hint) = &self.hint {
            out.push_str(hint);
            out.push('\n');
        }
        out
    }
}

/// What an action gives back: its report as JSON and as text, the exit
/// code, and the secrets the report may show because it was asked to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Outcome {
    pub(crate) json: Value,
    pub(crate) text: String,
    pub(crate) code: u8,
    pub(crate) reveal: Vec<String>,
}

/// A command's report: serialized for `--json` and a tool's structured
/// result, and rendered as text.
pub(crate) trait Report: Serialize {
    /// The report for people.
    fn text(&self) -> String;
}

impl Outcome {
    /// `report`, with exit code 0.
    pub(crate) fn of<R: Report>(report: &R) -> Self {
        Self {
            json: serde_json::to_value(report).unwrap_or(Value::Null),
            text: report.text(),
            code: exit::OK,
            reveal: Vec::new(),
        }
    }

    /// With exit code `code`.
    pub(crate) fn code(mut self, code: u8) -> Self {
        self.code = code;
        self
    }

    /// Allowed to show `secret`.
    pub(crate) fn reveal(mut self, secret: String) -> Self {
        self.reveal.push(secret);
        self
    }
}

/// A command, with its input.
#[derive(Clone, Debug)]
pub(crate) enum Command {
    Status,
    ConfigGet(config::GetInput),
    ConfigSet(config::SetInput),
    ConfigUnset(config::UnsetInput),
    ConfigShow,
    ConfigDiff,
    ConfigUndo,
    ConfigReplace(config::ReplaceInput),
    KeysList(keys::ListInput),
    KeysAdd(keys::AddInput),
    KeysRemove(keys::RemoveInput),
    CredentialsList(credentials::ListInput),
    CredentialsEnable(credentials::TargetInput),
    CredentialsDisable(credentials::TargetInput),
    CredentialsResetQuota(credentials::TargetInput),
    CredentialsRemove(credentials::TargetInput),
    CredentialsLogin(credentials::LoginInput),
    ClientsSetup(clients::SetupInput),
}

/// Runs `command` and scrubs what it gives back: of the secrets in the
/// config before and after it, `MANAGEMENT_PASSWORD` and the key file,
/// all but those the report was asked to show.
pub(crate) async fn perform(ctx: &Context, command: Command) -> Result<Outcome, Failure> {
    let mut secrets = mask::known_secrets(ctx);
    let result = run(ctx, command).await;
    secrets.extend(&mask::known_secrets(ctx));
    match result {
        Ok(mut outcome) => {
            let scrub = mask::Scrub::new(&secrets, &outcome.reveal);
            outcome.json = scrub.json(outcome.json);
            outcome.text = scrub.text(outcome.text);
            Ok(outcome)
        }
        Err(mut failure) => {
            let scrub = mask::Scrub::new(&secrets, &[]);
            failure.message = scrub.text(failure.message);
            failure.hint = failure.hint.map(|hint| scrub.text(hint));
            failure.would = failure.would.map(|would| scrub.json(would));
            Err(failure)
        }
    }
}

/// Runs `command`.
async fn run(ctx: &Context, command: Command) -> Result<Outcome, Failure> {
    match command {
        Command::Status => status::status(ctx).await,
        Command::ConfigGet(input) => config::get(ctx, &input),
        Command::ConfigSet(input) => config::set(ctx, input).await,
        Command::ConfigUnset(input) => config::unset(ctx, &input).await,
        Command::ConfigShow => config::show(ctx),
        Command::ConfigDiff => config::diff(ctx),
        Command::ConfigUndo => config::undo(ctx).await,
        Command::ConfigReplace(input) => config::replace(ctx, input).await,
        Command::KeysList(input) => keys::list(ctx, &input),
        Command::KeysAdd(input) => keys::add(ctx, input).await,
        Command::KeysRemove(input) => keys::remove(ctx, input).await,
        Command::CredentialsList(input) => credentials::list(ctx, &input).await,
        Command::CredentialsEnable(input) => credentials::set_disabled(ctx, &input, false).await,
        Command::CredentialsDisable(input) => credentials::set_disabled(ctx, &input, true).await,
        Command::CredentialsResetQuota(input) => credentials::reset_quota(ctx, &input).await,
        Command::CredentialsRemove(input) => credentials::remove(ctx, &input).await,
        Command::CredentialsLogin(input) => credentials::login(ctx, &input).await,
        Command::ClientsSetup(input) => clients::setup(ctx, &input).await,
    }
}
