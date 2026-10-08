//! The command line of `status`, `config`, `keys`, `credentials`,
//! `clients` and `mcp`: reads the flags into a command's input, runs it
//! (see [`super::perform`]) and prints its report, as text or, with
//! `--json`, as JSON.
//!
//! Flags take one dash or two (`-json`, `--json`), and a value after `=`
//! or as the next argument; `--` ends them. The exit code says how it went
//! (see [`super::exit`]).

use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::Value;

use super::clients::{CLIENTS, SetupInput, Shell};
use super::config::{GetInput, ReplaceInput, SetInput, Source, UnsetInput};
use super::credentials::{ListInput as CredentialsList, LoginInput, STATES, TargetInput};
use super::keys::{AddInput, ListInput as KeysList, RemoveInput};
use super::mask::is_secret_name;
use super::target::{Env, KEY_HINT, config_path};
use super::{Caller, Command, Context, Failure, exit, perform};

/// The most read from standard input.
const STDIN_LIMIT: u64 = 1024 * 1024;

/// The flags that take a value.
const VALUE_FLAGS: [&str; 10] = [
    "config",
    "expect-sha256",
    "management-key-file",
    "from-file",
    "to-file",
    "state",
    "provider",
    "model",
    "shell",
    "key-index",
];

/// The flags that take none.
const BOOL_FLAGS: [&str; 8] = [
    "json",
    "yes",
    "help",
    "from-stdin",
    "string",
    "reveal",
    "generate",
    "no-wait",
];

/// Flags that would put a secret on the command line: refused.
const SECRET_FLAGS: [&str; 5] = [
    "management-key",
    "management-password",
    "password",
    "secret-key",
    "api-key",
];

/// The flags every command takes.
const GLOBAL_FLAGS: [&str; 5] = ["config", "management-key-file", "json", "yes", "help"];

/// The arguments, read.
#[derive(Debug, Default)]
struct Args {
    positionals: Vec<String>,
    flags: BTreeMap<String, Option<String>>,
}

impl Args {
    fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }

    fn value(&self, name: &str) -> Option<String> {
        self.flags.get(name).cloned().flatten()
    }
}

/// Why the arguments can't be read.
enum ArgsError {
    Usage(String),
    /// A flag that would carry a secret: the flag, as [`flag_name`] shows
    /// it, and whether it is one for the management key.
    Secret(String, bool),
}

/// The longest flag name shown back.
const SHOWN_FLAG: usize = 32;

/// How the flag `name` is shown back in a failure: `--name` when it is a
/// short one of lowercase letters, digits and dashes, else `a flag`, so a
/// value pasted where a flag goes is never repeated.
fn flag_name(name: &str) -> String {
    let plain = !name.is_empty()
        && name.len() <= SHOWN_FLAG
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if plain {
        format!("--{name}")
    } else {
        "a flag".to_owned()
    }
}

/// Reads `args`, the arguments after the command.
fn parse_args(args: &[String]) -> Result<Args, ArgsError> {
    let mut parsed = Args::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--" {
            parsed.positionals.extend(rest.by_ref().cloned());
            break;
        }
        let looks_like_number = arg.parse::<f64>().is_ok();
        let Some(flag) = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-')) else {
            parsed.positionals.push(arg.clone());
            continue;
        };
        if looks_like_number || flag.is_empty() {
            parsed.positionals.push(arg.clone());
            continue;
        }
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (flag, None),
        };
        let name = match name {
            "y" => "yes",
            "h" | "?" => "help",
            other => other,
        };
        let known = VALUE_FLAGS.contains(&name) || BOOL_FLAGS.contains(&name);
        if SECRET_FLAGS.contains(&name) {
            return Err(ArgsError::Secret(flag_name(name), true));
        }
        if !known && is_secret_name(name) {
            return Err(ArgsError::Secret(flag_name(name), false));
        }
        if VALUE_FLAGS.contains(&name) {
            let value = match inline {
                Some(value) => value,
                None => rest
                    .next()
                    .cloned()
                    .ok_or_else(|| ArgsError::Usage(format!("--{name} needs a value")))?,
            };
            parsed.flags.insert(name.to_owned(), Some(value));
        } else if BOOL_FLAGS.contains(&name) {
            if inline.is_some() {
                return Err(ArgsError::Usage(format!("--{name} takes no value")));
            }
            parsed.flags.insert(name.to_owned(), None);
        } else {
            return Err(ArgsError::Usage(format!(
                "unknown flag: {}",
                flag_name(name)
            )));
        }
    }
    Ok(parsed)
}

/// The usage of these commands.
pub(crate) fn usage(program: &str) -> String {
    format!(
        "Usage: {program} <command> [flags]\n\
         \n\
         Commands to look at and change a setup, for people and agents:\n\
         \n\
         \x20 status                          whether a server runs for the config, and how it is\n\
         \x20 config get <path>               a setting, or that it isn't set and its default\n\
         \x20 config set <path> [value]       sets a setting to a YAML or JSON value\n\
         \x20     --from-stdin | --from-file <file>   read the value (a secret must come this way)\n\
         \x20     --string                    take the value as a string, as it is\n\
         \x20 config unset <path>             removes a setting\n\
         \x20 config show                     the whole config, masked\n\
         \x20 config diff                     what the last change made, against <config>.bak\n\
         \x20 config undo                     reverses the last change; run again to redo it\n\
         \x20 config replace --from-stdin | --from-file <file>   replaces the whole config\n\
         \x20 keys list [--reveal]            the client keys, masked (in full with --reveal --yes)\n\
         \x20 keys add --generate [--to-file <file>] | --from-stdin | --from-file <file>\n\
         \x20 keys remove <index> | --from-stdin | --from-file <file>\n\
         \x20 credentials list [--state <state>] [--provider <provider>]\n\
         \x20 credentials enable|disable|reset-quota|remove <auth_index or name>\n\
         \x20 credentials login <codex|claude> [--no-wait] [--state <state>]\n\
         \x20 clients setup <client> [--model <model>] [--shell posix|powershell]\n\
         \x20     [--key-index <n>] [--reveal]   prints a client's setup; clients: {clients}\n\
         \x20 mcp                             serves these commands as MCP tools on stdio\n\
         \n\
         Flags every command takes:\n\
         \x20 --config <file>                 the config (else config.yaml here, else the installed one)\n\
         \x20 --management-key-file <file>    a file holding the management key\n\
         \x20 --json                          print JSON\n\
         \x20 --yes, -y                       go ahead with a change that needs a confirmation\n\
         \x20 --expect-sha256 <hash>          with a change: make it only if the config's SHA-256 is\n\
         \x20                                 <hash>, the config_sha256 the change gave when it needed --yes\n\
         \n\
         The management key: {KEY_HINT}.\n\
         \n\
         Exit codes: 0 done; 1 failed; 2 bad usage; 3 needs --yes, or declined; 4 the server isn't running.\n\
         States: {states}.\n",
        clients = CLIENTS.join(", "),
        states = STATES.join(", "),
    )
}

/// Runs the command `args` names: `args` holds it and what follows it.
pub(crate) fn main(program: &str, args: Vec<String>) -> ExitCode {
    let Some((command, rest)) = args.split_first() else {
        eprint!("{}", usage(program));
        return ExitCode::from(exit::USAGE);
    };
    let json = rest.iter().any(|arg| arg == "--json" || arg == "-json");
    let parsed = match parse_args(rest) {
        Ok(parsed) => parsed,
        Err(ArgsError::Usage(message)) => {
            return fail(
                &Failure::usage(message).hint(format!("see `{program} {command} --help`")),
                json,
            );
        }
        Err(ArgsError::Secret(flag, management)) => {
            return fail(
                &Failure::new(
                    "secret_in_argument",
                    format!("{flag}: a secret is never taken from the command line, where it would be seen and kept in the shell's history; nothing was run"),
                )
                .hint(if management {
                    KEY_HINT
                } else {
                    "give a secret with --from-stdin or --from-file; the management key comes from the config, MANAGEMENT_PASSWORD or --management-key-file"
                }),
                json,
            );
        }
    };
    if parsed.has("help") {
        print!("{}", usage(program));
        return ExitCode::SUCCESS;
    }
    // This is before any thread starts, as setting a variable needs.
    let working_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(error) => {
            return fail(
                &Failure::new(
                    "failed",
                    format!("can't read the working directory: {error}"),
                ),
                json,
            );
        }
    };
    let _ = crate::load_dotenv(&working_dir.join(".env"));
    let flag = parsed.value("config").unwrap_or_default();
    let path = config_path(&flag, &working_dir, crate::installed::config_path());
    let key_file = parsed.value("management-key-file").map(PathBuf::from);
    let env = Env::current();
    if command == "mcp" {
        if let Err(failure) = allowed(&parsed, &[]) {
            return fail(&failure, json);
        }
        if !parsed.positionals.is_empty() {
            return fail(&Failure::usage("mcp takes no arguments"), json);
        }
        return super::mcp::main(path, env, key_file);
    }
    let path = match path {
        Ok(path) => path,
        Err(failure) => return fail(&failure, json),
    };
    let built = match build(command, &parsed) {
        Ok(built) => built,
        Err(failure) => return fail(&failure, json),
    };
    let expect_sha256 = match parsed
        .value("expect-sha256")
        .map(|text| super::change::parse_sha256(&text, "--expect-sha256"))
        .transpose()
    {
        Ok(expected) => expected,
        Err(failure) => return fail(&failure, json),
    };
    let from_stdin = parsed.has("from-stdin");
    let ask: Option<super::Ask> =
        (!from_stdin && std::io::stdin().is_terminal() && std::io::stderr().is_terminal())
            .then(|| Box::new(ask_at_terminal) as super::Ask);
    let ctx = Context {
        path,
        env,
        key_file,
        yes: parsed.has("yes"),
        expect_sha256,
        ask,
        say: Some(Box::new(|text: &str| {
            eprint!("{text}");
        })),
        caller: Caller::Cli,
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return fail(
                &Failure::new("failed", format!("can't start the async runtime: {error}")),
                json,
            );
        }
    };
    match runtime.block_on(perform(&ctx, built)) {
        Ok(outcome) => {
            if json {
                println!("{}", pretty(&outcome.json));
            } else {
                print!("{}", outcome.text);
            }
            let _ = std::io::stdout().flush();
            ExitCode::from(outcome.code)
        }
        Err(failure) => fail(&failure, json),
    }
}

/// Prints `failure` and gives its exit code: as JSON on standard output
/// with `--json`, else as text on standard error.
fn fail(failure: &Failure, json: bool) -> ExitCode {
    if json {
        let value = serde_json::to_value(failure).unwrap_or(Value::Null);
        println!("{}", pretty(&value));
    } else {
        eprint!("{}", failure.text());
    }
    ExitCode::from(failure.code)
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Asks `question` on standard error and reads the answer: yes for `y`
/// or `yes`.
fn ask_at_terminal(question: &str) -> bool {
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// A failure unless every flag in `parsed` is a global one or in `extra`.
fn allowed(parsed: &Args, extra: &[&str]) -> Result<(), Failure> {
    for name in parsed.flags.keys() {
        if !GLOBAL_FLAGS.contains(&name.as_str()) && !extra.contains(&name.as_str()) {
            return Err(Failure::usage(format!(
                "--{name} doesn't go with this command"
            )));
        }
    }
    Ok(())
}

/// The positionals of `parsed`, `count` of them, else a usage failure that
/// says they are `what`.
fn positionals(parsed: &Args, count: usize, what: &str) -> Result<Vec<String>, Failure> {
    if parsed.positionals.len() == count {
        Ok(parsed.positionals.clone())
    } else {
        Err(Failure::usage(format!("this command takes {what}")))
    }
}

/// Standard input, read.
fn read_stdin() -> Result<String, Failure> {
    let mut data = Vec::new();
    std::io::stdin()
        .take(STDIN_LIMIT + 1)
        .read_to_end(&mut data)
        .map_err(|error| Failure::new("failed", format!("can't read standard input: {error}")))?;
    if u64::try_from(data.len()).unwrap_or(u64::MAX) > STDIN_LIMIT {
        return Err(Failure::usage("standard input holds more than 1 MiB"));
    }
    String::from_utf8(data).map_err(|_| Failure::usage("standard input isn't UTF-8"))
}

/// Where a value comes from: `--from-stdin`, `--from-file`, or `argument`.
fn source(parsed: &Args, argument: Option<String>) -> Result<Option<Source>, Failure> {
    let file = parsed.value("from-file");
    let stdin = parsed.has("from-stdin");
    match (argument, stdin, file) {
        (None, false, None) => Ok(None),
        (Some(argument), false, None) => Ok(Some(Source::Argument(argument))),
        (None, true, None) => read_stdin().map(|text| Some(Source::Stdin(text))),
        (None, false, Some(file)) => Ok(Some(Source::File(PathBuf::from(file)))),
        _ => Err(Failure::usage(
            "give the value one way: as an argument, with --from-stdin, or with --from-file",
        )),
    }
}

/// The command `command` and `parsed` name.
fn build(command: &str, parsed: &Args) -> Result<Command, Failure> {
    let mut words = parsed.positionals.iter().map(String::as_str);
    let sub = match command {
        "status" => None,
        _ => words.next(),
    };
    let after = Args {
        positionals: parsed
            .positionals
            .iter()
            .skip(usize::from(sub.is_some()))
            .cloned()
            .collect(),
        flags: parsed.flags.clone(),
    };
    let parsed = &after;
    match (command, sub) {
        ("status", _) => {
            allowed(parsed, &[])?;
            positionals(parsed, 0, "no arguments")?;
            Ok(Command::Status)
        }
        ("config", Some("get")) => {
            allowed(parsed, &[])?;
            let [path] = <[String; 1]>::try_from(positionals(parsed, 1, "a setting's path")?)
                .map_err(|_| Failure::usage("this command takes a setting's path"))?;
            Ok(Command::ConfigGet(GetInput { path }))
        }
        ("config", Some("set")) => {
            allowed(
                parsed,
                &["from-stdin", "from-file", "string", "expect-sha256"],
            )?;
            let mut words = parsed.positionals.clone().into_iter();
            let path = words.next().ok_or_else(|| {
                Failure::usage("config set takes a setting's path, then its value")
            })?;
            let argument = words.next();
            if words.next().is_some() {
                return Err(Failure::usage(
                    "config set takes a setting's path and one value; quote a value with spaces",
                ));
            }
            let value = source(parsed, argument)?.ok_or_else(|| {
                Failure::usage(
                    "give the value as an argument, with --from-stdin, or with --from-file",
                )
            })?;
            Ok(Command::ConfigSet(SetInput {
                path,
                value,
                string: parsed.has("string"),
            }))
        }
        ("config", Some("unset")) => {
            allowed(parsed, &["expect-sha256"])?;
            let [path] = <[String; 1]>::try_from(positionals(parsed, 1, "a setting's path")?)
                .map_err(|_| Failure::usage("this command takes a setting's path"))?;
            Ok(Command::ConfigUnset(UnsetInput { path }))
        }
        ("config", Some(name @ ("show" | "diff" | "undo"))) => {
            let extra: &[&str] = if name == "undo" {
                &["expect-sha256"]
            } else {
                &[]
            };
            allowed(parsed, extra)?;
            positionals(parsed, 0, "no arguments")?;
            Ok(match name {
                "show" => Command::ConfigShow,
                "diff" => Command::ConfigDiff,
                _ => Command::ConfigUndo,
            })
        }
        ("config", Some("replace")) => {
            allowed(parsed, &["from-stdin", "from-file", "expect-sha256"])?;
            positionals(
                parsed,
                0,
                "no arguments: the config comes with --from-stdin or --from-file",
            )?;
            let source = source(parsed, None)?.ok_or_else(|| {
                Failure::usage("give the config with --from-stdin or --from-file")
            })?;
            Ok(Command::ConfigReplace(ReplaceInput { source }))
        }
        ("keys", Some("list")) => {
            allowed(parsed, &["reveal"])?;
            positionals(parsed, 0, "no arguments")?;
            Ok(Command::KeysList(KeysList {
                reveal: parsed.has("reveal"),
            }))
        }
        ("keys", Some("add")) => {
            allowed(
                parsed,
                &[
                    "generate",
                    "from-stdin",
                    "from-file",
                    "to-file",
                    "expect-sha256",
                ],
            )?;
            let argument = parsed.positionals.first().cloned();
            if parsed.positionals.len() > 1 {
                return Err(Failure::usage("keys add takes no arguments"));
            }
            Ok(Command::KeysAdd(AddInput {
                generate: parsed.has("generate"),
                source: source(parsed, argument)?,
                to_file: parsed.value("to-file").map(PathBuf::from),
            }))
        }
        ("keys", Some("remove")) => {
            allowed(parsed, &["from-stdin", "from-file", "expect-sha256"])?;
            if parsed.positionals.len() > 1 {
                return Err(Failure::usage("keys remove takes one index"));
            }
            let (index, argument) = match parsed.positionals.first() {
                Some(word) => match word.parse::<usize>() {
                    Ok(index) => (Some(index), None),
                    Err(_) => (None, Some(word.clone())),
                },
                None => (None, None),
            };
            let source = source(parsed, argument)?;
            if index.is_some() && source.is_some() {
                return Err(Failure::usage(
                    "name the key by its index or give it, not both",
                ));
            }
            Ok(Command::KeysRemove(RemoveInput { index, source }))
        }
        ("credentials", Some("list")) => {
            allowed(parsed, &["state", "provider"])?;
            positionals(parsed, 0, "no arguments")?;
            Ok(Command::CredentialsList(CredentialsList {
                state: parsed.value("state"),
                provider: parsed.value("provider"),
            }))
        }
        ("credentials", Some(name @ ("enable" | "disable" | "reset-quota" | "remove"))) => {
            allowed(parsed, &[])?;
            let [credential] = <[String; 1]>::try_from(positionals(
                parsed,
                1,
                "a credential's auth_index or name",
            )?)
            .map_err(|_| Failure::usage("this command takes a credential's auth_index or name"))?;
            let input = TargetInput { credential };
            Ok(match name {
                "enable" => Command::CredentialsEnable(input),
                "disable" => Command::CredentialsDisable(input),
                "reset-quota" => Command::CredentialsResetQuota(input),
                _ => Command::CredentialsRemove(input),
            })
        }
        ("credentials", Some("login")) => {
            allowed(parsed, &["state", "no-wait"])?;
            let [provider] =
                <[String; 1]>::try_from(positionals(parsed, 1, "a provider: codex or claude")?)
                    .map_err(|_| {
                        Failure::usage("this command takes a provider: codex or claude")
                    })?;
            Ok(Command::CredentialsLogin(LoginInput {
                provider,
                state: parsed.value("state"),
                wait: !parsed.has("no-wait"),
            }))
        }
        ("clients", Some("setup")) => {
            allowed(parsed, &["model", "shell", "key-index", "reveal"])?;
            let [client] = <[String; 1]>::try_from(positionals(
                parsed,
                1,
                &format!("a client: one of {}", CLIENTS.join(", ")),
            )?)
            .map_err(|_| Failure::usage("this command takes a client"))?;
            let shell = parsed
                .value("shell")
                .map(|shell| Shell::parse(&shell))
                .transpose()?;
            let key_index = parsed
                .value("key-index")
                .map(|index| {
                    index
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| Failure::usage("--key-index takes a number, from 0"))
                })
                .transpose()?;
            Ok(Command::ClientsSetup(SetupInput {
                client,
                model: parsed.value("model"),
                shell,
                key_index,
                reveal: parsed.has("reveal"),
            }))
        }
        (_, None) => {
            Err(Failure::usage(format!("{command} needs a subcommand")).hint(subcommands(command)))
        }
        (_, Some(other)) => Err(
            Failure::usage(format!("unknown command: {command} {other}"))
                .hint(subcommands(command)),
        ),
    }
}

/// The subcommands of `command`.
fn subcommands(command: &str) -> String {
    let names = match command {
        "config" => "get, set, unset, show, diff, undo and replace",
        "keys" => "list, add and remove",
        "credentials" => "list, enable, disable, reset-quota, remove and login",
        "clients" => "setup",
        _ => "none",
    };
    format!("its subcommands: {names}; see `open-ferry {command} --help`")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    // Not upstream's: flags take one dash or two, a value after = or as the
    // next argument; -- ends them, and a negative number is a value.
    #[test]
    fn reads_flags() {
        let Ok(parsed) = parse_args(&args(&[
            "set",
            "-json",
            "--config=a.yaml",
            "--from-file",
            "v.yaml",
            "-y",
            "x",
            "-5",
            "--",
            "--json",
        ])) else {
            panic!("didn't parse");
        };
        assert_eq!(parsed.positionals, ["set", "x", "-5", "--json"]);
        assert!(parsed.has("json") && parsed.has("yes"));
        assert_eq!(parsed.value("config").unwrap(), "a.yaml");
        assert_eq!(parsed.value("from-file").unwrap(), "v.yaml");
        assert!(matches!(
            parse_args(&args(&["--nope"])),
            Err(ArgsError::Usage(_))
        ));
        assert!(matches!(
            parse_args(&args(&["--json=1"])),
            Err(ArgsError::Usage(_))
        ));
        assert!(matches!(
            parse_args(&args(&["--config"])),
            Err(ArgsError::Usage(_))
        ));
    }

    // Not upstream's: a flag that would carry the management key, or whose
    // name names a secret, is refused, with or without its value.
    #[test]
    fn refuses_key_flags() {
        for (flag, management) in [
            ("--management-key=abc", true),
            ("--management-key", true),
            ("-password", true),
            ("--secret-key=x", true),
            ("--token=tok-value-0123456789", false),
            ("--access-token", false),
            ("--client-secret=x", false),
            ("--openai-api-key=sk-abc", false),
            ("--cookie=session=abc", false),
        ] {
            match parse_args(&args(&[flag])) {
                Err(ArgsError::Secret(shown, which)) => {
                    assert_eq!(which, management, "{flag}");
                    assert!(!shown.contains('='), "{flag}: {shown}");
                    assert!(!shown.contains("abc") && !shown.contains("0123"));
                }
                _ => panic!("{flag} wasn't refused"),
            }
        }
        // The flags these commands take aren't secrets, though one names
        // the key's file.
        assert!(parse_args(&args(&["--management-key-file", "k"])).is_ok());
    }

    // Not upstream's: an unknown flag is named, never with its value, and
    // a long or odd name isn't shown at all.
    #[test]
    fn unknown_flags_are_named_alone() {
        let message = |flag: &str| match parse_args(&args(&[flag])) {
            Err(ArgsError::Usage(message)) => message,
            _ => panic!("{flag} wasn't refused"),
        };
        assert_eq!(message("--nope=value-0123"), "unknown flag: --nope");
        assert_eq!(message("-key=sk-abc-0123"), "unknown flag: --key");
        assert_eq!(
            message("--sk-proj-abcdefghijklmnopqrstuvwxyz0123456789"),
            "unknown flag: a flag"
        );
        assert_eq!(message("--Bearer_ABC"), "unknown flag: a flag");
    }

    // Not upstream's: each command's arguments, and the flags that don't
    // go with it.
    #[test]
    fn builds_commands() {
        let command = |words: &[&str]| {
            let Ok(parsed) = parse_args(&args(&words[1..])) else {
                panic!("didn't parse");
            };
            build(words[0], &parsed)
        };
        assert!(matches!(command(&["status"]), Ok(Command::Status)));
        assert!(matches!(
            command(&["config", "get", "server.port"]),
            Ok(Command::ConfigGet(_))
        ));
        assert!(matches!(
            command(&["config", "set", "server.port", "1"]),
            Ok(Command::ConfigSet(SetInput {
                value: Source::Argument(_),
                ..
            }))
        ));
        assert!(matches!(
            command(&["keys", "remove", "2"]),
            Ok(Command::KeysRemove(RemoveInput {
                index: Some(2),
                source: None
            }))
        ));
        assert!(matches!(
            command(&["keys", "remove", "sk-abc"]),
            Ok(Command::KeysRemove(RemoveInput {
                index: None,
                source: Some(Source::Argument(_))
            }))
        ));
        assert!(matches!(
            command(&["credentials", "login", "codex", "--no-wait"]),
            Ok(Command::CredentialsLogin(LoginInput { wait: false, .. }))
        ));
        assert_eq!(
            command(&["status", "--reveal"]).unwrap_err().code,
            exit::USAGE
        );
        assert_eq!(command(&["config"]).unwrap_err().code, exit::USAGE);
        assert_eq!(command(&["config", "nope"]).unwrap_err().code, exit::USAGE);
        assert_eq!(
            command(&["config", "set", "a"]).unwrap_err().code,
            exit::USAGE
        );
        assert_eq!(
            command(&["config", "set", "a", "b", "c"]).unwrap_err().code,
            exit::USAGE
        );
        assert_eq!(
            command(&["clients", "setup", "curl", "--shell", "fish"])
                .unwrap_err()
                .code,
            exit::USAGE
        );
    }
}
