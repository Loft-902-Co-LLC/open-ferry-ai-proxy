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
//! Deviations from upstream:
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
    /// Use only the built-in model catalog (`-local-model`).
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

enum Kind {
    Bool(fn(&mut Flags) -> &mut bool),
    String(fn(&mut Flags) -> &mut String),
    Int(fn(&mut Flags) -> &mut i64),
}

struct Definition {
    name: &'static str,
    usage: &'static str,
    kind: Kind,
}

/// The flags, sorted by name as Go's usage lists them.
const DEFINITIONS: [Definition; 11] = [
    Definition {
        name: "claude-login",
        usage: "Login to Claude using OAuth",
        kind: Kind::Bool(|flags| &mut flags.claude_login),
    },
    Definition {
        name: "codex-device-login",
        usage: "Login to Codex using device code flow",
        kind: Kind::Bool(|flags| &mut flags.codex_device_login),
    },
    Definition {
        name: "codex-login",
        usage: "Login to Codex using OAuth",
        kind: Kind::Bool(|flags| &mut flags.codex_login),
    },
    Definition {
        name: "config",
        usage: "Configure File Path",
        kind: Kind::String(|flags| &mut flags.config),
    },
    Definition {
        name: "local-model",
        usage: "Use the embedded model catalogs only (remote catalog updates and catalog sources aren't ported, so this is always so)",
        kind: Kind::Bool(|flags| &mut flags.local_model),
    },
    Definition {
        name: "management-base-url",
        usage: "Base URL of remote management API for TUI client mode (e.g. https://proxy.example.com)",
        kind: Kind::String(|flags| &mut flags.management_base_url),
    },
    Definition {
        name: "no-browser",
        usage: "Don't open browser automatically for OAuth",
        kind: Kind::Bool(|flags| &mut flags.no_browser),
    },
    Definition {
        name: "oauth-callback-port",
        usage: "Override OAuth callback port (defaults to provider-specific port)",
        kind: Kind::Int(|flags| &mut flags.oauth_callback_port),
    },
    Definition {
        name: "password",
        usage: "",
        kind: Kind::String(|flags| &mut flags.password.0),
    },
    Definition {
        name: "standalone",
        usage: "In TUI mode, start an embedded local server",
        kind: Kind::Bool(|flags| &mut flags.standalone),
    },
    Definition {
        name: "tui",
        usage: "Start with terminal management UI",
        kind: Kind::Bool(|flags| &mut flags.tui),
    },
];

/// Reads `args`, without the program name.
pub fn parse<I>(args: I) -> Result<Flags, FlagError>
where
    I: IntoIterator<Item = String>,
{
    let mut flags = Flags::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Some(body) = arg.strip_prefix('-').filter(|body| !body.is_empty()) else {
            break;
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
        let Some(definition) = DEFINITIONS.iter().find(|d| d.name == name) else {
            if matches!(name, "h" | "help") {
                return Err(FlagError::Help);
            }
            return Err(FlagError::Invalid(format!(
                "flag provided but not defined: -{name}"
            )));
        };
        match definition.kind {
            Kind::Bool(field) => {
                *field(&mut flags) = match value {
                    None => true,
                    Some(value) => parse_bool(value).ok_or_else(|| {
                        FlagError::Invalid(format!(
                            "invalid boolean value {value:?} for -{name}: parse error"
                        ))
                    })?,
                };
            }
            Kind::String(field) => *field(&mut flags) = value_of(name, value, &mut args)?,
            Kind::Int(field) => {
                let value = value_of(name, value, &mut args)?;
                *field(&mut flags) = value.parse().map_err(|_| {
                    FlagError::Invalid(format!(
                        "invalid value {value:?} for flag -{name}: parse error"
                    ))
                })?;
            }
        }
    }
    Ok(flags)
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
    for definition in &DEFINITIONS {
        if definition.name == "password" {
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
    out
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
}
