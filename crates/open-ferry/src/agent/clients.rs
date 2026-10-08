//! `open-ferry clients setup <client>`: the lines that point a client at
//! the proxy, with the address, a client key and a model filled in, as
//! the dashboard's overview page writes them (its `snippets.ts`). It only
//! prints them; it never writes a client's settings.
//!
//! The routes and models come from the running server's client setup
//! (`GET /open-ferry/api/v1/client-setup`); with no server, from the
//! config's address and the proxy's usual routes, with no model. The key
//! is the config's first client key (or the one `--key-index` names),
//! masked unless `--reveal --yes` is given on the command line; a tool
//! never shows it.

use std::collections::BTreeMap;

use axum::http::Method;
use open_ferry_dashboard::mask_client_key;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::guard::confirm;
use super::target::{Reach, probe};
use super::values::{get, read_tree};
use super::{Caller, Context, Failure, Outcome, Report};

/// The prompt each setup sends.
const PROMPT: &str = "Say hello.";

/// What a setup names while there is no model.
const MODEL_PLACEHOLDER: &str = "<model>";

/// What a setup names while the config has no client key.
const KEY_PLACEHOLDER: &str = "<your client key>";

/// The environment variable the Codex setup reads the key from.
const CODEX_KEY_VARIABLE: &str = "OPEN_FERRY_API_KEY";

/// The shell a setup's commands are for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Shell {
    Posix,
    Powershell,
}

impl Shell {
    /// The shell named `name`.
    pub(crate) fn parse(name: &str) -> Result<Self, Failure> {
        match name.trim().to_ascii_lowercase().as_str() {
            "posix" | "bash" | "zsh" | "sh" => Ok(Self::Posix),
            "powershell" | "pwsh" => Ok(Self::Powershell),
            other => Err(Failure::usage(format!(
                "unknown shell {other}: posix or powershell"
            ))),
        }
    }

    /// This system's usual shell.
    fn default_here() -> Self {
        if cfg!(windows) {
            Self::Powershell
        } else {
            Self::Posix
        }
    }
}

/// `clients setup`'s input.
#[derive(Clone, Debug, Default)]
pub(crate) struct SetupInput {
    /// The client: one of [`CLIENTS`].
    pub(crate) client: String,
    /// The model to name, else a suggestion.
    pub(crate) model: Option<String>,
    /// The shell, else this system's usual one.
    pub(crate) shell: Option<Shell>,
    /// Which client key to use, from 0.
    pub(crate) key_index: Option<usize>,
    /// Show the key in full: the command line only, with `--yes`.
    pub(crate) reveal: bool,
}

/// The clients there is a setup for.
pub(crate) const CLIENTS: [&str; 5] = [
    "openai-python",
    "openai-node",
    "codex",
    "claude-code",
    "curl",
];

/// One of the proxy's routes, as the client setup gives it.
#[derive(Clone, Debug, Deserialize)]
struct Route {
    protocol: String,
    #[serde(default)]
    base_path: String,
    #[serde(default)]
    models: Vec<String>,
}

/// A model, as the client setup describes it.
#[derive(Clone, Debug, Deserialize)]
struct ModelInfo {
    id: String,
    #[serde(default)]
    owned_by: String,
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    chat: bool,
}

/// The models a client is made for.
struct Suits {
    /// The `owned_by` of its maker's models.
    maker: &'static str,
    /// How its maker's model names start, after any prefix.
    family: &'static str,
}

/// A setup's client.
struct Setup {
    id: &'static str,
    label: &'static str,
    protocol: &'static str,
    suits: Option<Suits>,
}

const SETUPS: [Setup; 5] = [
    Setup {
        id: "openai-python",
        label: "OpenAI SDK (Python)",
        protocol: "openai",
        suits: None,
    },
    Setup {
        id: "openai-node",
        label: "OpenAI SDK (Node)",
        protocol: "openai",
        suits: None,
    },
    Setup {
        id: "codex",
        label: "Codex CLI",
        protocol: "openai-responses",
        suits: Some(Suits {
            maker: "openai",
            family: "gpt",
        }),
    },
    Setup {
        id: "claude-code",
        label: "Claude Code",
        protocol: "claude",
        suits: Some(Suits {
            maker: "anthropic",
            family: "claude",
        }),
    },
    Setup {
        id: "curl",
        label: "curl",
        protocol: "openai",
        suits: None,
    },
];

/// A part of a setup: what to do with it, and its lines.
#[derive(Debug, Serialize)]
struct Part {
    caption: String,
    code: String,
}

/// A client's setup.
#[derive(Debug, Serialize)]
struct ClientSetup {
    client: &'static str,
    label: &'static str,
    base_url: String,
    model: String,
    /// The model asked for isn't on the client's route now.
    model_missing: bool,
    /// The key as the setup shows it: masked, in full, or a placeholder.
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_index: Option<usize>,
    revealed: bool,
    shell: Shell,
    /// The environment variables the setup sets.
    env: BTreeMap<&'static str, String>,
    parts: Vec<Part>,
    /// The documentation it follows.
    source: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
}

impl Report for ClientSetup {
    fn text(&self) -> String {
        let mut out = format!(
            "{}, through {}, with the model {}:\n",
            self.label, self.base_url, self.model
        );
        for part in &self.parts {
            let caption = part.caption.trim_end_matches(['.', ':']);
            out.push_str(&format!("\n{caption}:\n\n"));
            for line in part.code.lines() {
                out.push_str(&format!("    {line}\n"));
            }
        }
        out.push_str(&format!("\nIt follows {}\n", self.source));
        for note in &self.notes {
            out.push_str(&format!("Note: {note}\n"));
        }
        out
    }
}

// ---------------------------------------------------------------- quoting

/// A double-quoted string for JavaScript, Python or JSON.
fn quoted(value: &str) -> String {
    Value::String(value.to_owned()).to_string()
}

/// A TOML basic string: JSON's escapes, and DEL escaped too.
fn toml_string(value: &str) -> String {
    let escape = format!("{}u007f", '\x5c');
    quoted(value).replace('\x7f', &escape)
}

/// A single-quoted POSIX shell word: nothing in it is special.
fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A single-quoted PowerShell string. PowerShell takes the curly single
/// quotes as quotes too, so they are doubled as well.
fn powershell_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        out.push(c);
        if c == '\'' || (0x2018..=0x201b).contains(&u32::from(c)) {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn shell_string(value: &str, shell: Shell) -> String {
    match shell {
        Shell::Posix => shell_word(value),
        Shell::Powershell => powershell_string(value),
    }
}

fn set_env(name: &str, value: &str, shell: Shell) -> String {
    match shell {
        Shell::Posix => format!("export {name}={}", shell_word(value)),
        Shell::Powershell => format!("$env:{name} = {}", powershell_string(value)),
    }
}

// ------------------------------------------------------------------ models

fn suits_client(info: &ModelInfo, suits: &Suits) -> bool {
    let name = info
        .id
        .rsplit('/')
        .next()
        .unwrap_or(&info.id)
        .to_lowercase();
    info.owned_by.to_lowercase() == suits.maker || name.starts_with(suits.family)
}

/// The model to suggest on `route`: of its chat models, those that suit
/// the client if any do, the one that came out last. A model whose date
/// is unknown counts as oldest, and of models with the same date the first
/// on the route wins. Without a chat model, the route's first model.
fn suggested_model(route: &Route, models: &[ModelInfo], suits: Option<&Suits>) -> Option<String> {
    let chat: Vec<&ModelInfo> = route
        .models
        .iter()
        .filter_map(|id| models.iter().find(|info| &info.id == id))
        .filter(|info| info.chat)
        .collect();
    let suited: Vec<&ModelInfo> = suits
        .map(|suits| {
            chat.iter()
                .copied()
                .filter(|info| suits_client(info, suits))
                .collect()
        })
        .unwrap_or_default();
    let pool = if suited.is_empty() { chat } else { suited };
    let mut newest: Option<&ModelInfo> = None;
    for info in pool {
        let newer = newest
            .is_none_or(|best| info.created.unwrap_or(i64::MIN) > best.created.unwrap_or(i64::MIN));
        if newer {
            newest = Some(info);
        }
    }
    newest
        .map(|info| info.id.clone())
        .or_else(|| route.models.first().cloned())
}

/// The model a setup names, and whether the one picked is missing.
fn model_for(
    route: &Route,
    picked: Option<&str>,
    models: &[ModelInfo],
    suits: Option<&Suits>,
) -> (String, bool) {
    if let Some(picked) = picked
        && route.models.iter().any(|model| model == picked)
    {
        return (picked.to_owned(), false);
    }
    let suggested = suggested_model(route, models, suits);
    match picked {
        None => (
            suggested.unwrap_or_else(|| MODEL_PLACEHOLDER.to_owned()),
            false,
        ),
        Some(picked) => (suggested.unwrap_or_else(|| picked.to_owned()), true),
    }
}

fn chat_body(model: &str) -> String {
    json!({"model": model, "messages": [{"role": "user", "content": PROMPT}]}).to_string()
}

// ------------------------------------------------------------------ setups

/// The parts of `setup`'s lines, and the documentation they follow.
fn build(id: &str, key: &str, base: &str, model: &str, shell: Shell) -> (Vec<Part>, &'static str) {
    let part = |caption: &str, lines: Vec<String>| Part {
        caption: caption.to_owned(),
        code: lines.join("\n"),
    };
    match id {
        "openai-python" => {
            let source = "https://github.com/openai/openai-python#usage";
            (
                vec![part(
                    "Python, with the openai package installed",
                    vec![
                        format!("# The OpenAI Python SDK, pointed at open-ferry: {source}"),
                        "from openai import OpenAI".to_owned(),
                        String::new(),
                        "client = OpenAI(".to_owned(),
                        format!("    base_url={},", quoted(base)),
                        format!("    api_key={},", quoted(key)),
                        ")".to_owned(),
                        "completion = client.chat.completions.create(".to_owned(),
                        format!("    model={},", quoted(model)),
                        format!(
                            "    messages=[{{\"role\": \"user\", \"content\": {}}}],",
                            quoted(PROMPT)
                        ),
                        ")".to_owned(),
                        "print(completion.choices[0].message.content)".to_owned(),
                    ],
                )],
                source,
            )
        }
        "openai-node" => {
            let source = "https://github.com/openai/openai-node#usage";
            (
                vec![part(
                    "JavaScript (an ES module), with the openai package installed",
                    vec![
                        format!("// The OpenAI Node SDK, pointed at open-ferry: {source}"),
                        "import OpenAI from \"openai\";".to_owned(),
                        String::new(),
                        "const client = new OpenAI({".to_owned(),
                        format!("  baseURL: {},", quoted(base)),
                        format!("  apiKey: {},", quoted(key)),
                        "});".to_owned(),
                        "const completion = await client.chat.completions.create({".to_owned(),
                        format!("  model: {},", quoted(model)),
                        format!(
                            "  messages: [{{ role: \"user\", content: {} }}],",
                            quoted(PROMPT)
                        ),
                        "});".to_owned(),
                        "console.log(completion.choices[0].message.content);".to_owned(),
                    ],
                )],
                source,
            )
        }
        "codex" => {
            let source = "https://github.com/openai/codex/blob/main/docs/config.md#model_providers";
            (
                vec![
                    part(
                        "Add to ~/.codex/config.toml. The first two lines go above any [table] in it.",
                        vec![
                            format!("# Codex CLI through open-ferry: {source}"),
                            format!("model = {}", toml_string(model)),
                            "model_provider = \"open-ferry\"".to_owned(),
                            String::new(),
                            "[model_providers.open-ferry]".to_owned(),
                            "name = \"open-ferry\"".to_owned(),
                            format!("base_url = {}", toml_string(base)),
                            format!("env_key = \"{CODEX_KEY_VARIABLE}\""),
                            "wire_api = \"responses\"".to_owned(),
                        ],
                    ),
                    part(
                        &format!("Then start Codex with the key in {CODEX_KEY_VARIABLE}"),
                        vec![set_env(CODEX_KEY_VARIABLE, key, shell), "codex".to_owned()],
                    ),
                ],
                source,
            )
        }
        "claude-code" => {
            let source = "https://docs.claude.com/en/docs/claude-code/llm-gateway";
            (
                vec![part(
                    "Start Claude Code with these set",
                    vec![
                        format!("# Claude Code through open-ferry, as an LLM gateway: {source}"),
                        set_env("ANTHROPIC_BASE_URL", base, shell),
                        set_env("ANTHROPIC_AUTH_TOKEN", key, shell),
                        set_env("ANTHROPIC_MODEL", model, shell),
                        "claude".to_owned(),
                    ],
                )],
                source,
            )
        }
        _ => {
            let source = "https://platform.openai.com/docs/api-reference/chat/create";
            let url = format!("{base}/chat/completions");
            let lines = match shell {
                Shell::Posix => vec![
                    format!("# A chat completion, as OpenAI's API takes it: {source}"),
                    format!("curl {} \\", shell_word(&url)),
                    format!(
                        "  -H {} \\",
                        shell_word(&format!("Authorization: Bearer {key}"))
                    ),
                    "  -H 'Content-Type: application/json' \\".to_owned(),
                    format!("  -d {}", shell_word(&chat_body(model))),
                ],
                Shell::Powershell => vec![
                    format!("# A chat completion, as OpenAI's API takes it: {source}"),
                    format!(
                        "Invoke-RestMethod -Method Post -Uri {} `",
                        powershell_string(&url)
                    ),
                    format!(
                        "  -Headers @{{ Authorization = {} }} `",
                        powershell_string(&format!("Bearer {key}"))
                    ),
                    "  -ContentType 'application/json' `".to_owned(),
                    format!("  -Body {}", shell_string(&chat_body(model), shell)),
                ],
            };
            (vec![part("Run in a terminal", lines)], source)
        }
    }
}

/// The environment variables `id`'s setup sets.
fn env_of(id: &str, key: &str, base: &str, model: &str) -> BTreeMap<&'static str, String> {
    let mut env = BTreeMap::new();
    match id {
        "openai-python" | "openai-node" => {
            env.insert("OPENAI_BASE_URL", base.to_owned());
            env.insert("OPENAI_API_KEY", key.to_owned());
        }
        "codex" => {
            env.insert(CODEX_KEY_VARIABLE, key.to_owned());
        }
        "claude-code" => {
            env.insert("ANTHROPIC_BASE_URL", base.to_owned());
            env.insert("ANTHROPIC_AUTH_TOKEN", key.to_owned());
            env.insert("ANTHROPIC_MODEL", model.to_owned());
        }
        _ => {}
    }
    env
}

/// The routes when no server tells them: the proxy's usual ones, with no
/// models.
fn fallback_routes() -> Vec<Route> {
    [
        ("claude", ""),
        ("openai", "/v1"),
        ("openai-responses", "/v1"),
    ]
    .into_iter()
    .map(|(protocol, base_path)| Route {
        protocol: protocol.to_owned(),
        base_path: base_path.to_owned(),
        models: Vec::new(),
    })
    .collect()
}

/// `clients setup`.
pub(crate) async fn setup(ctx: &Context, input: &SetupInput) -> Result<Outcome, Failure> {
    let id = input.client.trim().to_ascii_lowercase();
    let Some(setup) = SETUPS.iter().find(|setup| setup.id == id) else {
        return Err(Failure::usage(format!(
            "there is no setup for {}: one of {}",
            input.client.trim(),
            CLIENTS.join(", ")
        )));
    };
    if input.reveal && ctx.caller == Caller::Mcp {
        return Err(Failure::usage("no tool shows a client key in full").hint(
            "the person fills the key in, or runs `open-ferry clients setup <client> --reveal --yes` in a terminal",
        ));
    }
    let tree = read_tree(&ctx.path)?;
    let keys: Vec<String> = get(&tree, &["access".to_owned(), "api-keys".to_owned()])
        .and_then(Value::as_array)
        .map(|keys| {
            keys.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut notes = Vec::new();
    let (key, key_index) = match input.key_index {
        Some(index) => match keys.get(index) {
            Some(key) => (Some(key.clone()), Some(index)),
            None => {
                return Err(Failure::new(
                    "not_found",
                    format!(
                        "there is no client key {index}: access.api-keys has {}",
                        keys.len()
                    ),
                ));
            }
        },
        None => (keys.first().cloned(), keys.first().map(|_| 0)),
    };
    if input.reveal && key.is_some() {
        confirm(
            ctx,
            "Showing the client key in full",
            &["it prints a secret".to_owned()],
            json!({}),
            &[],
        )?;
    }
    let shown = match &key {
        None => {
            let make = match ctx.caller {
                Caller::Cli => "`open-ferry keys add --generate`",
                Caller::Mcp => "keys_add with generate: true",
            };
            notes.push(format!(
                "the config has no client key: make one with {make}, then put it where {KEY_PLACEHOLDER} is"
            ));
            KEY_PLACEHOLDER.to_owned()
        }
        Some(key) if input.reveal => key.clone(),
        Some(key) => {
            notes.push(match ctx.caller {
                Caller::Cli => "the client key is masked: put the key in its place, or run this again with --reveal --yes to fill it in".to_owned(),
                Caller::Mcp => "the client key is masked: the person puts the key in its place".to_owned(),
            });
            mask_client_key(key)
        }
    };
    let target = probe(ctx).await?;
    let (root, routes, models) = match &target.reach {
        Reach::Running(server) => {
            let answer = server
                .remote
                .json(Method::GET, "/open-ferry/api/v1/client-setup", None)
                .await?;
            let routes: Vec<Route> = answer
                .get("routes")
                .cloned()
                .and_then(|routes| serde_json::from_value(routes).ok())
                .unwrap_or_default();
            let models: Vec<ModelInfo> = answer
                .get("models")
                .cloned()
                .and_then(|models| serde_json::from_value(models).ok())
                .unwrap_or_default();
            let root = answer
                .get("base_urls")
                .and_then(Value::as_array)
                .and_then(|urls| urls.first())
                .and_then(|url| url.get("url"))
                .and_then(Value::as_str)
                .map(|url| url.trim_end_matches('/').to_owned())
                .or_else(|| target.proxy_url.clone());
            (root, routes, models)
        }
        other => {
            if let Reach::OtherConfig(failure) = other {
                notes.push(failure.message.clone());
            }
            notes.push(
                "no server answered with its routes and models, so the model is a placeholder; with the server running, a model it serves is filled in".to_owned(),
            );
            (target.proxy_url.clone(), fallback_routes(), Vec::new())
        }
    };
    let Some(root) = root else {
        return Err(Failure::new(
            "invalid_config",
            "the config sets no server.port, so the proxy has no address to point a client at",
        ));
    };
    let Some(route) = routes.iter().find(|route| route.protocol == setup.protocol) else {
        return Err(Failure::new(
            "unavailable",
            format!(
                "the server has no {} route for {}",
                setup.protocol, setup.label
            ),
        ));
    };
    let (model, model_missing) =
        model_for(route, input.model.as_deref(), &models, setup.suits.as_ref());
    if model_missing {
        notes.push(format!(
            "the model asked for isn't on this route now, so {model} is named instead"
        ));
    }
    let base = format!("{root}{}", route.base_path);
    let shell = input.shell.unwrap_or_else(Shell::default_here);
    let (parts, source) = build(setup.id, &shown, &base, &model, shell);
    let report = ClientSetup {
        client: setup.id,
        label: setup.label,
        base_url: base.clone(),
        model: model.clone(),
        model_missing,
        key: shown.clone(),
        key_index,
        revealed: input.reveal && key.is_some(),
        shell,
        env: env_of(setup.id, &shown, &base, &model),
        parts,
        source,
        notes,
    };
    let mut outcome = Outcome::of(&report);
    if report.revealed
        && let Some(key) = key
    {
        outcome = outcome.reveal(key);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: the quoting matches the dashboard's snippets.ts.
    #[test]
    fn quotes_as_the_dashboard_does() {
        assert_eq!(shell_word("it's"), "'it'\\''s'");
        assert_eq!(powershell_string("it's"), "'it''s'");
        let curly = char::from_u32(0x2019).unwrap();
        assert_eq!(
            powershell_string(&format!("a{curly}b")),
            format!("'a{curly}{curly}b'")
        );
        let del = format!("a{}b", '\x7f');
        assert_eq!(toml_string(&del), format!("\"a{}u007fb\"", '\x5c'));
        assert_eq!(set_env("A", "b", Shell::Posix), "export A='b'");
        assert_eq!(set_env("A", "b", Shell::Powershell), "$env:A = 'b'");
    }

    // Not upstream's: the suggestion prefers the client's maker's newest
    // chat model, and a missing pick names the suggestion.
    #[test]
    fn suggests_models() {
        let info = |id: &str, owned_by: &str, created: Option<i64>, chat: bool| ModelInfo {
            id: id.to_owned(),
            owned_by: owned_by.to_owned(),
            created,
            chat,
        };
        let models = vec![
            info("gpt-old", "openai", Some(1), true),
            info("gpt-new", "openai", Some(5), true),
            info("other", "x", Some(9), true),
            info("image", "openai", Some(10), false),
        ];
        let route = Route {
            protocol: "openai-responses".to_owned(),
            base_path: "/v1".to_owned(),
            models: vec![
                "gpt-old".into(),
                "gpt-new".into(),
                "other".into(),
                "image".into(),
            ],
        };
        let suits = Suits {
            maker: "openai",
            family: "gpt",
        };
        assert_eq!(
            suggested_model(&route, &models, Some(&suits)).unwrap(),
            "gpt-new"
        );
        assert_eq!(suggested_model(&route, &models, None).unwrap(), "other");
        assert_eq!(
            model_for(&route, Some("gpt-old"), &models, Some(&suits)),
            ("gpt-old".to_owned(), false)
        );
        assert_eq!(
            model_for(&route, Some("nope"), &models, Some(&suits)),
            ("gpt-new".to_owned(), true)
        );
        let empty = Route {
            protocol: "openai".to_owned(),
            base_path: "/v1".to_owned(),
            models: Vec::new(),
        };
        assert_eq!(
            model_for(&empty, None, &[], None),
            (MODEL_PLACEHOLDER.to_owned(), false)
        );
        assert_eq!(
            model_for(&empty, Some("m"), &[], None),
            ("m".to_owned(), true)
        );
    }
}
