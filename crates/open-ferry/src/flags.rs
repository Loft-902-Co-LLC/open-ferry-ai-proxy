// Ported from the flag definitions in CLIProxyAPI cmd/server/main.go, read
// the way Go's flag package reads them (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The command line.
//!
//! As with Go's `flag` package, a flag is `-name` or `--name`, a value comes
//! as `-name=value` or as the next argument, a boolean flag takes only
//! `-name=value`, and parsing stops at the first argument that isn't a flag
//! or after `--`. Later arguments are ignored, as upstream ignores them.
//! The usage leaves out `-password`, as upstream's does.
//!
//! A subcommand (`open-ferry init`, `open-ferry check`) is recognized only
//! as the first argument; `main` sends the arguments after it to the
//! subcommand, which reads its own flags with the same parser
//! ([`parse_with`]) and has its own `-h`. Without one, or with a flag
//! first, the command line is read as above.
//!
//! Deviations from upstream:
//! - Upstream has no subcommands: it ignores a first argument that isn't a
//!   flag, and serves. Here `init` and `check` as the first argument run
//!   those subcommands.
//! - Only the flags of the ported features are defined: `-config`, the Codex
//!   and Claude logins, `-no-browser`, `-oauth-callback-port`,
//!   `-local-model`, `-password`, and the TUI's `-tui`, `-standalone` and
//!   `-management-base-url`. Any other flag is an error, as an unknown flag
//!   is upstream.
//! - `-oauth-callback-port` takes a decimal number; Go also takes `0x`, `0o`
//!   and `0b` prefixes and underscores.

use std::fmt::{self, Write as _};

/// What the command line asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// The config file, or empty for `config.yaml` in the working directory
    /// (`-config`).
    pub config: String,
    /// Log in to Codex in a browser (`-codex-login`).
    pub codex_login: bool,
    /// Log in to Codex with a device code (`-codex-device-login`).
    pub codex_device_login: bool,
    /// Log in to Claude in a browser (`-claude-login`).
    pub claude_login: bool,
    /// Show the login URL rather than open a browser (`-no-browser`).
    pub no_browser: bool,
    /// The port for the login's callback server, or 0 for the provider's
    /// (`-oauth-callback-port`).
    pub oauth_callback_port: i64,
    /// Use the built-in model catalogs unless the config names a catalog
    /// file (`-local-model`), as without it.
    pub local_model: bool,
    /// The local management password, or empty (`-password`).
    pub password: Password,
    /// Run the terminal management UI (`-tui`).
    pub tui: bool,
    /// Run the TUI with a server of its own (`-standalone`).
    pub standalone: bool,
    /// The management API the TUI uses, or empty for the config's
    /// (`-management-base-url`).
    pub management_base_url: String,
}

/// A password from the command line, which `Debug` doesn't show.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Password(pub String);

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(..)")
    }
}

/// Why the command line didn't parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlagError {
    /// `-h` or `-help`: show the usage and exit with success.
    Help,
    /// A bad flag: show this message and the usage, and exit with 2.
    Invalid(String),
}

/// What a flag takes, and how its value is set.
pub enum Kind<T> {
    /// A boolean: `-name`, or `-name=value`.
    Bool(fn(&mut T, bool)),
    /// A string.
    String(fn(&mut T, String)),
    /// A decimal integer.
    Int(fn(&mut T, i64)),
}

/// One flag of a command: its name, its line in the usage, and what it
/// takes.
pub struct Definition<T> {
    /// The name, without the leading `-`.
    pub name: &'static str,
    /// What the usage says of it.
    pub usage: &'static str,
    /// What it takes.
    pub kind: Kind<T>,
}

/// The flags, sorted by name as Go's usage lists them.
const DEFINITIONS: [Definition<Flags>; 11] = [
    Definition {
        name: "claude-login",
        usage: "Login to Claude using OAuth",
        kind: Kind::Bool(|flags, value| flags.claude_login = value),
    },
    Definition {
        name: "codex-device-login",
        usage: "Login to Codex using device code flow",
        kind: Kind::Bool(|flags, value| flags.codex_device_login = value),
    },
    Definition {
        name: "codex-login",
        usage: "Login to Codex using OAuth",
        kind: Kind::Bool(|flags, value| flags.codex_login = value),
    },
    Definition {
        name: "config",
        usage: "Configure File Path",
        kind: Kind::String(|flags, value| flags.config = value),
    },
    Definition {
        name: "local-model",
        usage: "Use embedded model catalogs unless models.catalog or models.codex-catalog names a file (no catalog is downloaded, so this is always so)",
        kind: Kind::Bool(|flags, value| flags.local_model = value),
    },
    Definition {
        name: "management-base-url",
        usage: "Base URL of remote management API for TUI client mode (e.g. https://proxy.example.com)",
        kind: Kind::String(|flags, value| flags.management_base_url = value),
    },
    Definition {
        name: "no-browser",
        usage: "Don't open browser automatically for OAuth",
        kind: Kind::Bool(|flags, value| flags.no_browser = value),
    },
    Definition {
        name: "oauth-callback-port",
        usage: "Override OAuth callback port (defaults to provider-specific port)",
        kind: Kind::Int(|flags, value| flags.oauth_callback_port = value),
    },
    Definition {
        name: "password",
        usage: "",
        kind: Kind::String(|flags, value| flags.password.0 = value),
    },
    Definition {
        name: "standalone",
        usage: "In TUI mode, start an embedded local server",
        kind: Kind::Bool(|flags, value| flags.standalone = value),
    },
    Definition {
        name: "tui",
        usage: "Start with terminal management UI",
        kind: Kind::Bool(|flags, value| flags.tui = value),
    },
];

/// Reads `args`, without the program name.
pub fn parse<I>(args: I) -> Result<Flags, FlagError>
where
    I: IntoIterator<Item = String>,
{
    parse_with(&DEFINITIONS, args).map(|(flags, _)| flags)
}

/// Reads `args` as the flags of `definitions` define them, into a `T` that
/// starts as its default, and returns it with the arguments after the
/// flags: those from the first that isn't a flag, or after `--`.
pub fn parse_with<T, I>(
    definitions: &[Definition<T>],
    args: I,
) -> Result<(T, Vec<String>), FlagError>
where
    T: Default,
    I: IntoIterator<Item = String>,
{
    let mut flags = T::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Some(body) = arg.strip_prefix('-').filter(|body| !body.is_empty()) else {
            return Ok((flags, std::iter::once(arg).chain(args).collect()));
        };
        let body = match body.strip_prefix('-') {
            Some("") => break,
            Some(rest) => rest,
            None => body,
        };
        if body.starts_with('-') || body.starts_with('=') {
            return Err(FlagError::Invalid(format!("bad flag syntax: {arg}")));
        }
        let (name, value) = match body.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (body, None),
        };
        let Some(definition) = definitions.iter().find(|d| d.name == name) else {
            if matches!(name, "h" | "help") {
                return Err(FlagError::Help);
            }
            return Err(FlagError::Invalid(format!(
                "flag provided but not defined: -{name}"
            )));
        };
        match definition.kind {
            Kind::Bool(set) => {
                let value = match value {
                    None => true,
                    Some(value) => parse_bool(value).ok_or_else(|| {
                        FlagError::Invalid(format!(
                            "invalid boolean value {value:?} for -{name}: parse error"
                        ))
                    })?,
                };
                set(&mut flags, value);
            }
            Kind::String(set) => set(&mut flags, value_of(name, value, &mut args)?),
            Kind::Int(set) => {
                let value = value_of(name, value, &mut args)?;
                let value = value.parse().map_err(|_| {
                    FlagError::Invalid(format!(
                        "invalid value {value:?} for flag -{name}: parse error"
                    ))
                })?;
                set(&mut flags, value);
            }
        }
    }
    Ok((flags, args.collect()))
}

fn value_of(
    name: &str,
    value: Option<&str>,
    args: &mut impl Iterator<Item = String>,
) -> Result<String, FlagError> {
    match value {
        Some(value) => Ok(value.to_owned()),
        None => args
            .next()
            .ok_or_else(|| FlagError::Invalid(format!("flag needs an argument: -{name}"))),
    }
}

/// Go's `strconv.ParseBool`.
fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// The usage text, as Go's `flag.PrintDefaults` writes it.
pub fn usage(program: &str) -> String {
    let mut out = format!("Usage of {program}:\n");
    write_defaults(&mut out, &DEFINITIONS, &["password"]);
    out
}

/// Writes a line for each of `definitions` but the `hidden` ones, as Go's
/// `flag.PrintDefaults` writes it.
pub fn write_defaults<T>(out: &mut String, definitions: &[Definition<T>], hidden: &[&str]) {
    for definition in definitions {
        if hidden.contains(&definition.name) {
            continue;
        }
        let kind = match definition.kind {
            Kind::Bool(_) => "",
            Kind::String(_) => " string",
            Kind::Int(_) => " int",
        };
        let _ = writeln!(
            out,
            "  -{}{kind}\n    \t{}",
            definition.name, definition.usage
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Flags, FlagError> {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn reads_flags_as_go_does() {
        let flags = parse_args(&[
            "-codex-login",
            "--config",
            "c.yaml",
            "-oauth-callback-port=1456",
            "-no-browser=false",
        ])
        .unwrap();
        assert_eq!(
            flags,
            Flags {
                config: "c.yaml".into(),
                codex_login: true,
                oauth_callback_port: 1456,
                ..Flags::default()
            }
        );
        let flags = parse_args(&[
            "-tui",
            "-standalone=true",
            "-password",
            "pw",
            "--management-base-url=https://proxy.example.com",
        ])
        .unwrap();
        assert_eq!(
            flags,
            Flags {
                tui: true,
                standalone: true,
                password: Password("pw".into()),
                management_base_url: "https://proxy.example.com".into(),
                ..Flags::default()
            }
        );
        assert_eq!(format!("{:?}", flags.password), "Password(..)");
        let flags = parse_args(&["--claude-login", "rest", "-codex-login"]).unwrap();
        assert!(flags.claude_login && !flags.codex_login);
        let flags = parse_args(&["--", "-codex-login"]).unwrap();
        assert!(!flags.codex_login);
        let flags = parse_args(&["-", "-codex-login"]).unwrap();
        assert!(!flags.codex_login);
        assert_eq!(parse_args(&["-config="]).unwrap().config, "");
    }

    #[test]
    fn turns_down_bad_flags_as_go_does() {
        let invalid = |args: &[&str]| match parse_args(args) {
            Err(FlagError::Invalid(message)) => message,
            other => panic!("{args:?}: {other:?}"),
        };
        assert_eq!(
            invalid(&["-kimi-login"]),
            "flag provided but not defined: -kimi-login"
        );
        assert_eq!(invalid(&["-config"]), "flag needs an argument: -config");
        assert_eq!(invalid(&["---config"]), "bad flag syntax: ---config");
        assert_eq!(invalid(&["-=x"]), "bad flag syntax: -=x");
        assert_eq!(
            invalid(&["-no-browser=yes"]),
            "invalid boolean value \"yes\" for -no-browser: parse error"
        );
        assert_eq!(
            invalid(&["-oauth-callback-port", "x"]),
            "invalid value \"x\" for flag -oauth-callback-port: parse error"
        );
        assert_eq!(parse_args(&["-h"]), Err(FlagError::Help));
        assert_eq!(parse_args(&["--help"]), Err(FlagError::Help));
    }

    #[test]
    fn usage_lists_the_flags_as_go_does() {
        let usage = usage("open-ferry");
        assert!(usage.starts_with("Usage of open-ferry:\n  -claude-login\n    \tLogin to Claude"));
        assert!(usage.contains("\n  -config string\n    \tConfigure File Path\n"));
        assert!(usage.contains("\n  -oauth-callback-port int\n"));
        assert!(
            usage.contains(
                "\n  -management-base-url string\n    \tBase URL of remote management API"
            )
        );
        assert!(usage.contains("\n  -standalone\n    \tIn TUI mode,"));
        assert!(usage.ends_with("\n  -tui\n    \tStart with terminal management UI\n"));
        assert!(!usage.contains("password"));
    }

    // Not upstream's: a subcommand's flags, read with the same parser, and
    // the arguments after them.
    #[test]
    fn reads_a_subcommands_flags_and_returns_the_rest() {
        #[derive(Default)]
        struct Sub {
            force: bool,
            name: String,
        }
        let definitions = [
            Definition {
                name: "force",
                usage: "Replace it",
                kind: Kind::Bool(|sub: &mut Sub, value| sub.force = value),
            },
            Definition {
                name: "name",
                usage: "The name",
                kind: Kind::String(|sub: &mut Sub, value| sub.name = value),
            },
        ];
        let read =
            |args: &[&str]| parse_with(&definitions, args.iter().map(|arg| (*arg).to_owned()));
        let (sub, rest) = read(&["-force", "-name", "x", "extra", "-force=false"]).unwrap();
        assert!(sub.force);
        assert_eq!(sub.name, "x");
        assert_eq!(rest, ["extra", "-force=false"]);
        let (sub, rest) = read(&["--", "-force"]).unwrap();
        assert!(!sub.force);
        assert_eq!(rest, ["-force"]);
        let (_, rest) = read(&[]).unwrap();
        assert!(rest.is_empty());
        assert!(matches!(
            read(&["-config", "x"]),
            Err(FlagError::Invalid(_))
        ));
        assert!(matches!(read(&["-h"]), Err(FlagError::Help)));

        let mut out = String::new();
        write_defaults(&mut out, &definitions, &[]);
        assert_eq!(
            out,
            "  -force\n    \tReplace it\n  -name string\n    \tThe name\n"
        );
    }
}
