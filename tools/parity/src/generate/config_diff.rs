//! Seeded random config pairs for the config change details: a config
//! with a random mix of the settings open-ferry types, and a second one
//! that is the same, a fresh one, or the first with a few sections, fields
//! of a key entry or OAuth channels changed or removed. Values aim at the
//! diff's corners: padded and differently cased names that normalize to the
//! same entry, URLs with user information, paths and odd hosts, values
//! parsing clamps, raw payload JSON, and model lists in another order.
//!
//! What the generator avoids, as Go and open-ferry can't agree on it or Go
//! doesn't agree with itself:
//! - Empty lists and mappings, and raw payload rules that parsing drops:
//!   Go tells a missing list from an empty one in the payload sections.
//! - Two OAuth channels or two header names that are the same once trimmed
//!   (or, for channels, in lower case): which one Go keeps depends on its
//!   map order.
//! - A management key that isn't bcrypt-shaped: Go hashes it with a random
//!   salt while parsing, so it differs from itself.

use serde_json::{Map, Value, json};

use super::{Generator, Rng};
use crate::cases::Case;

const URLS: &[&str] = &[
    "",
    "http://proxy.example:3128",
    " http://proxy.example:3128 ",
    "https://user:pass@secure.example/v1?token=secret",
    "socks5://user:pass@192.168.1.1:1080/path",
    "socks5://proxy.example.com:1080/",
    "example.com:1234/path?x=1",
    "/just/path",
    "http://[::1",
    "http://[fe80::1%25en0]:8080/x",
    "HTTP://Upper.Example",
    "proxy.example",
    "http://ex%41mple.com/",
    "mailto:someone@example.com",
    "http://user@",
    "https://\u{fc}nicode.example/",
];

const AUTH_DIRS: &[&str] = &[
    "",
    "~/.cli-proxy-api",
    " /padded/auth ",
    "C:\\auth",
    "/tmp/auth",
];

const STRATEGIES: &[&str] = &["", "round-robin", "fill-first", " Fill-First ", "wrr"];

const TIMEOUTS: &[&str] = &["", "5s", " 5s ", "10s", "1m"];

const API_KEYS: &[&str] = &["key-1", " key-1 ", "key-2", "sk-\u{e9}t\u{e9}", "key-3"];

const PREFIXES: &[&str] = &["", "team", " team ", "/team/", "Team", "a/b"];

const MODEL_NAMES: &[&str] = &[
    "gpt-6-sol",
    "GPT-6-Sol",
    "gemini-2.5-pro",
    " gemini-2.5-pro ",
    "claude-sonnet-4-6",
    "m1",
    "m2",
    "\u{130}stanbul",
    "<b>&</b>",
    "",
];

const ALIASES: &[&str] = &["", "sol", "pro", "Pro", "a1", " a1 "];

const DISPLAY_NAMES: &[&str] = &["", "Display", " Display ", "<Display>", "\u{c9}t\u{e9}"];

const LEVELS: &[&str] = &["low", "medium", "high", "<high>", "\u{fc}ber"];

const HEADER_NAMES: &[&str] = &["X-Team", "x-team", "Authorization", " X-Pad ", "X-Empty"];

const HEADER_VALUES: &[&str] = &["a", "b", "Bearer secret", " padded ", ""];

const EXCLUDED: &[&str] = &[
    "gpt-5.5-mini",
    "GPT-5.5-MINI",
    " gemini-1.5-* ",
    "claude-2*",
    "*-preview",
    "<tag>",
    "",
];

const CHANNELS: &[&str] = &[
    "codex",
    "Codex",
    " claude ",
    "gemini-cli",
    "vertex",
    "antigravity",
    "qwen",
    "aistudio",
];

const ACTIONS: &[&str] = &["stop", "continue", ""];

const STATUSES: &[i64] = &[0, 400, 429, 500];

const SECRETS: &[&str] = &[
    "",
    "$2a$10$abcdefghijklmnopqrstuv",
    "$2b$10$zyxwvutsrqponmlkjihgfe",
    "$2y$12$another",
];

const REPOSITORIES: &[&str] = &[
    "",
    "https://github.com/example/panel",
    "https://user:pass@panel.example/private?token=t",
    " https://github.com/example/panel ",
    "repo-old",
];

const MODEL_RULE_NAMES: &[&str] = &["gpt-*", "*", "gemini-*", "claude-sonnet-4-6"];

const PROTOCOLS: &[&str] = &["", "openai", "gemini", "claude", "codex"];

const PARAM_PATHS: &[&str] = &[
    "reasoning.effort",
    "max_output_tokens",
    "store",
    "metadata",
    "generationConfig.thinkingConfig",
    "tools.0.name",
];

/// `disable-image-generation` values, as JSON: each mode as a switch, a
/// number, or a text in any case, with spaces around it, or empty.
const IMAGE_GENERATION: &[&str] = &[
    "false",
    "true",
    "0",
    "1",
    "\"\"",
    "\"true\"",
    "\"False\"",
    "\"chat\"",
    "\" Chat \"",
    "\"CHAT\"",
    "\"passthrough\"",
    "\" Passthrough \"",
    "\"yes\"",
    "\"off\"",
    "\"on\"",
    "\"no\"",
];

const RAW_JSON: &[&str] = &["{}", "[1, 2]", "{\"a\": 1}", " true ", "\"text\"", "null"];

/// The top-level keys the generator sets.
const SECTIONS: &[&str] = &[
    "port",
    "auth-dir",
    "debug",
    "logging-to-file",
    "usage-statistics-enabled",
    "redis-usage-queue-retention-seconds",
    "disable-cooling",
    "save-cooldown-status",
    "transient-error-cooldown-seconds",
    "disable-image-generation",
    "request-log",
    "logs-max-total-size-mb",
    "error-logs-max-files",
    "request-retry",
    "max-retry-credentials",
    "max-retry-interval",
    "proxy-url",
    "ws-auth",
    "force-model-prefix",
    "nonstream-keepalive-interval",
    "quota-exceeded",
    "codex",
    "xai",
    "client",
    "routing",
    "api-keys",
    "payload",
    "gemini-api-key",
    "interactions-api-key",
    "claude-api-key",
    "codex-api-key",
    "xai-api-key",
    "meta-api-key",
    "vertex-api-key",
    "oauth-excluded-models",
    "oauth-model-alias",
    "oauth-request-scoped-errors",
    "oauth-settings",
    "remote-management",
    "openai-compatibility",
];

/// Fields a tweak never removes from a list's entries, so an entry keeps
/// what makes it one.
const KEPT_FIELDS: &[&str] = &["api-key", "name", "base-url", "models", "params"];

/// Case options for `config-diff/details`.
pub fn detail_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Configs {
                rng: Generator::new(seed, index).rng,
            };
            let old = generator.config();
            let new = generator.changed(&old);
            Case::new(format!("random-{seed}-{index}"), "", "").with_options(json!({
                "old": yaml(&old),
                "new": yaml(&new),
            }))
        })
        .collect()
}

/// The random source for one case's configs.
struct Configs {
    rng: Rng,
}

impl Configs {
    /// A config with each section set or not.
    fn config(&mut self) -> Map<String, Value> {
        let mut config = Map::new();
        for section in SECTIONS {
            if self.rng.chance(35) {
                config.insert((*section).to_owned(), self.section(section));
            }
        }
        if config.is_empty() {
            config.insert("port".to_owned(), json!(8317));
        }
        config
    }

    /// `old` unchanged, a fresh config, or `old` with a few changes.
    fn changed(&mut self, old: &Map<String, Value>) -> Map<String, Value> {
        match self.rng.below(10) {
            0 => return old.clone(),
            1 => return self.config(),
            _ => {}
        }
        let mut new = old.clone();
        for _ in 0..=self.rng.below(3) {
            let section = self.rng.pick(SECTIONS);
            let fresh = self.section(section);
            let action = self.rng.below(4);
            match new.get_mut(section) {
                Some(current) if action < 2 => self.tweak(current, fresh),
                Some(_) if action == 2 => {
                    new.remove(section);
                }
                _ => {
                    new.insert(section.to_owned(), fresh);
                }
            }
        }
        if new.is_empty() {
            new.insert("port".to_owned(), json!(8317));
        }
        new
    }

    /// Changes one part of `current` to `fresh`'s: one field of a mapping
    /// (or of one entry of a list of mappings), or the whole value. A key
    /// of `fresh` that is another key of `current` once trimmed and in
    /// lower case (an OAuth channel or a header name) isn't added.
    fn tweak(&mut self, current: &mut Value, fresh: Value) {
        match (current, fresh) {
            (Value::Object(current), Value::Object(mut fresh)) => {
                let keys: Vec<String> = current.keys().chain(fresh.keys()).cloned().collect();
                let key = self.rng.pick(&keys);
                let normalized = key.trim().to_lowercase();
                let collides = current
                    .keys()
                    .any(|other| *other != key && other.trim().to_lowercase() == normalized);
                match fresh.remove(&key) {
                    Some(_) if collides => {}
                    Some(value) => {
                        current.insert(key, value);
                    }
                    None if current.len() > 1 && !KEPT_FIELDS.contains(&key.as_str()) => {
                        current.remove(&key);
                    }
                    None => {}
                }
            }
            (Value::Array(current), Value::Array(fresh)) => {
                let (Some(index), Some(entry)) = (
                    (!current.is_empty()).then(|| self.rng.below(current.len())),
                    fresh.into_iter().next(),
                ) else {
                    return;
                };
                if let Some(target) = current.get_mut(index) {
                    if target.is_object() && entry.is_object() && self.rng.chance(70) {
                        self.tweak(target, entry);
                    } else {
                        *target = entry;
                    }
                }
            }
            (current, fresh) => *current = fresh,
        }
    }

    /// A value for the top-level key `section`.
    fn section(&mut self, section: &str) -> Value {
        match section {
            "port" => json!(self.rng.pick(&[0, 1, 8080, 8317, 9090])),
            "auth-dir" => json!(self.rng.pick(AUTH_DIRS)),
            "redis-usage-queue-retention-seconds" => {
                json!(self.rng.pick(&[-1, 0, 30, 60, 3600, 5000]))
            }
            "transient-error-cooldown-seconds" => json!(self.rng.pick(&[-1, 0, 30])),
            "disable-image-generation" => {
                serde_json::from_str(self.rng.pick(IMAGE_GENERATION)).unwrap_or(Value::Null)
            }
            "logs-max-total-size-mb" => json!(self.rng.pick(&[-1, 0, 10, 512])),
            "error-logs-max-files" => json!(self.rng.pick(&[-2, 0, 5, 10])),
            "request-retry" => json!(self.rng.pick(&[0, 1, 3])),
            "max-retry-credentials" => json!(self.rng.pick(&[-1, 0, 1, 4])),
            "max-retry-interval" => json!(self.rng.pick(&[0, 1, 30])),
            "nonstream-keepalive-interval" => json!(self.rng.pick(&[0, 5, 15])),
            "proxy-url" => json!(self.rng.pick(URLS)),
            "quota-exceeded" => self.flags(&[
                "switch-project",
                "switch-preview-model",
                "antigravity-credits",
            ]),
            "codex" => {
                let mut codex = self.flags(&[
                    "stream-bootstrap-buffering",
                    "orphan-delegation-compatibility",
                ]);
                codex["stream-bootstrap-timeout"] = json!(self.rng.pick(TIMEOUTS));
                codex
            }
            "xai" => self.flags(&["inject-x-search"]),
            "client" => json!({
                "codex": self.flags(&["optimize-multi-agent-v2", "enable-apply-patch"]),
            }),
            "routing" => json!({ "strategy": self.rng.pick(STRATEGIES) }),
            "api-keys" => self.list(4, |generator| json!(generator.rng.pick(API_KEYS))),
            "payload" => self.payload(),
            "gemini-api-key"
            | "interactions-api-key"
            | "claude-api-key"
            | "codex-api-key"
            | "xai-api-key"
            | "meta-api-key"
            | "vertex-api-key" => self.list(3, |generator| generator.provider_key(section)),
            "oauth-excluded-models" => self.channels(|generator| {
                generator.list(3, |generator| json!(generator.rng.pick(EXCLUDED)))
            }),
            "oauth-model-alias" => self.channels(|generator| {
                generator.list(3, |generator| {
                    let mut alias = generator.flags(&["fork", "force-mapping"]);
                    alias["name"] = json!(generator.rng.pick(MODEL_NAMES));
                    alias["alias"] = json!(generator.rng.pick(ALIASES));
                    alias["display-name"] = json!(generator.rng.pick(DISPLAY_NAMES));
                    alias
                })
            }),
            "oauth-request-scoped-errors" => self.channels(|generator| {
                generator.list(3, |generator| {
                    let mut rule = json!({
                        "status": generator.rng.pick(STATUSES),
                        "action": generator.rng.pick(ACTIONS),
                    });
                    if generator.rng.chance(80) {
                        rule["match"] =
                            generator.list(2, |generator| json!(generator.rng.pick(EXCLUDED)));
                    }
                    if generator.rng.chance(30) {
                        rule["match-regexr"] = json!(["^context.*$"]);
                    }
                    rule
                })
            }),
            "oauth-settings" => self.channels(|generator| {
                generator.list(3, |generator| {
                    json!({
                        "name": generator.rng.pick(MODEL_NAMES),
                        "alias": generator.rng.pick(ALIASES),
                        "max-context-length": generator.rng.pick(&[0, 200_000, 272_000]),
                    })
                })
            }),
            "remote-management" => {
                let mut remote = self.flags(&[
                    "allow-remote",
                    "disable-control-panel",
                    "disable-auto-update-panel",
                ]);
                remote["secret-key"] = json!(self.rng.pick(SECRETS));
                remote["panel-github-repository"] = json!(self.rng.pick(REPOSITORIES));
                if self.rng.chance(50) {
                    remote["base-url"] = json!(self.rng.pick(URLS));
                }
                remote
            }
            "openai-compatibility" => self.list(3, Self::compat),
            // The switches.
            _ => json!(self.rng.chance(50)),
        }
    }

    /// One entry of a provider's key list.
    fn provider_key(&mut self, section: &str) -> Value {
        let mut key = Map::new();
        key.insert("api-key".to_owned(), json!(self.rng.pick(API_KEYS)));
        let mut field = |generator: &mut Self, name: &str, value: Value| {
            if generator.rng.chance(50) {
                key.insert(name.to_owned(), value);
            }
        };
        let url = json!(self.rng.pick(URLS));
        field(self, "base-url", url);
        let url = json!(self.rng.pick(URLS));
        field(self, "proxy-url", url);
        let prefix = json!(self.rng.pick(PREFIXES));
        field(self, "prefix", prefix);
        let headers = self.headers();
        field(self, "headers", headers);
        let excluded = self.list(3, |generator| json!(generator.rng.pick(EXCLUDED)));
        field(self, "excluded-models", excluded);
        let disable_cooling = json!(self.rng.chance(50));
        field(self, "disable-cooling", disable_cooling);
        let retry = json!(self.rng.pick(&[0, 2]));
        field(self, "request-retry", retry);
        let vertex = section == "vertex-api-key";
        let models = self.list(3, |generator| generator.model(vertex));
        field(self, "models", models);
        match section {
            "claude-api-key" => {
                let rebuild = json!(self.rng.chance(50));
                field(self, "rebuild-mid-system-message", rebuild);
            }
            "codex-api-key" | "xai-api-key" | "meta-api-key" => {
                // Only Codex shows `alpha-search` and only Meta leaves out
                // `websockets`, and only xAI and Meta show the priority;
                // each key gets them all, to check the lines left out stay
                // out.
                let websockets = json!(self.rng.chance(50));
                field(self, "websockets", websockets);
                let alpha_search = json!(self.rng.chance(50));
                field(self, "alpha-search", alpha_search);
                let priority = json!(self.rng.pick(&[-1, 0, 1, 5]));
                field(self, "priority", priority);
            }
            _ => {}
        }
        Value::Object(key)
    }

    /// A model of a provider key; Vertex models have no `is-compat`.
    fn model(&mut self, vertex: bool) -> Value {
        let mut model = json!({
            "name": self.rng.pick(MODEL_NAMES),
            "alias": self.rng.pick(ALIASES),
        });
        if self.rng.chance(40) {
            model["display-name"] = json!(self.rng.pick(DISPLAY_NAMES));
        }
        if self.rng.chance(30) {
            model["force-mapping"] = json!(self.rng.chance(50));
        }
        if !vertex && self.rng.chance(30) {
            model["is-compat"] = json!(self.rng.chance(50));
        }
        if self.rng.chance(30) {
            let mut thinking = json!({
                "min": self.rng.pick(&[0, 128, 1024]),
                "max": self.rng.pick(&[0, 8192, 32_768]),
            });
            if self.rng.chance(50) {
                thinking["zero-allowed"] = json!(self.rng.chance(50));
                thinking["dynamic-allowed"] = json!(self.rng.chance(50));
            }
            if self.rng.chance(50) {
                thinking["levels"] = self.list(3, |generator| json!(generator.rng.pick(LEVELS)));
            }
            model["thinking"] = thinking;
        }
        model
    }

    /// An `openai-compatibility` provider.
    fn compat(&mut self) -> Value {
        let mut compat = json!({
            "name": self.rng.pick(&["provider-a", "provider-b", " provider-a ", "", "dup"]),
            "base-url": self.rng.pick(&[
                "https://a.example/v1",
                "https://user:pass@b.example/v1?k=secret",
                " https://a.example/v1 ",
                "",
            ]),
        });
        if self.rng.chance(60) {
            compat["api-key-entries"] = self.list(
                3,
                |generator| json!({ "api-key": generator.rng.pick(&["key-a", "key-b", " "]) }),
            );
        }
        if self.rng.chance(60) {
            compat["models"] = self.list(3, |generator| {
                json!({
                    "name": generator.rng.pick(MODEL_NAMES),
                    "alias": generator.rng.pick(ALIASES),
                })
            });
        }
        if self.rng.chance(40) {
            compat["headers"] = self.headers();
        }
        for flag in ["disabled", "support-prompt-cache-key", "disable-cooling"] {
            if self.rng.chance(30) {
                compat[flag] = json!(self.rng.chance(50));
            }
        }
        if self.rng.chance(30) {
            compat["request-retry"] = json!(self.rng.pick(&[0, 2]));
        }
        compat
    }

    /// The payload sections, each set or not, but at least one.
    fn payload(&mut self) -> Value {
        let mut payload = Map::new();
        for section in [
            "default",
            "default-raw",
            "override",
            "override-raw",
            "filter",
        ] {
            if !self.rng.chance(45) {
                continue;
            }
            let rules = match section {
                "filter" => self.list(2, |generator| {
                    json!({
                        "models": generator.model_rules(),
                        "params": generator.list(2, |generator| {
                            json!(generator.rng.pick(PARAM_PATHS))
                        }),
                    })
                }),
                _ => {
                    let raw = section.ends_with("-raw");
                    self.list(2, |generator| {
                        json!({
                            "models": generator.model_rules(),
                            "params": generator.params(raw),
                        })
                    })
                }
            };
            payload.insert(section.to_owned(), rules);
        }
        if payload.is_empty() {
            payload.insert(
                "default".to_owned(),
                json!([{ "models": [{ "name": "*" }], "params": { "store": false } }]),
            );
        }
        Value::Object(payload)
    }

    fn model_rules(&mut self) -> Value {
        self.list(2, |generator| {
            let mut rule = json!({ "name": generator.rng.pick(MODEL_RULE_NAMES) });
            let protocol = generator.rng.pick(PROTOCOLS);
            if !protocol.is_empty() {
                rule["protocol"] = json!(protocol);
            }
            rule
        })
    }

    /// A rule's params; a raw rule's string values are valid JSON, as
    /// parsing drops the rule otherwise.
    fn params(&mut self, raw: bool) -> Value {
        let mut params = Map::new();
        for _ in 0..=self.rng.below(2) {
            let value = match self.rng.below(8) {
                0 | 1 if raw => json!(self.rng.pick(RAW_JSON)),
                0 => json!(self.rng.pick(&["high", "", "\u{fc}ber"])),
                1 => json!(self.rng.pick(&[0, 1, 4096, -7])),
                2 => json!(self.rng.pick(&[1.0, 0.5, -2.25])),
                3 => json!(self.rng.chance(50)),
                4 => Value::Null,
                5 => json!([1, "two", null]),
                6 => json!({ "nested": { "deep": true } }),
                _ => json!(self.rng.pick(&[1, 2])),
            };
            params.insert(self.rng.pick(PARAM_PATHS).to_owned(), value);
        }
        Value::Object(params)
    }

    /// Header names that stay distinct once trimmed, with their values.
    fn headers(&mut self) -> Value {
        let mut headers = Map::new();
        let mut seen = Vec::new();
        for _ in 0..=self.rng.below(3) {
            let name = self.rng.pick(HEADER_NAMES);
            if seen.contains(&name.trim()) {
                continue;
            }
            seen.push(name.trim());
            headers.insert(name.to_owned(), json!(self.rng.pick(HEADER_VALUES)));
        }
        Value::Object(headers)
    }

    /// An OAuth map: channels that stay distinct once trimmed and in lower
    /// case, each with what `entries` gives.
    fn channels(&mut self, mut entries: impl FnMut(&mut Self) -> Value) -> Value {
        let mut channels = Map::new();
        let mut seen = Vec::new();
        for _ in 0..=self.rng.below(3) {
            let channel = self.rng.pick(CHANNELS);
            let normalized = channel.trim().to_ascii_lowercase();
            if seen.contains(&normalized) {
                continue;
            }
            seen.push(normalized);
            channels.insert(channel.to_owned(), entries(self));
        }
        Value::Object(channels)
    }

    /// One to `max` items.
    fn list(&mut self, max: usize, mut item: impl FnMut(&mut Self) -> Value) -> Value {
        Value::Array((0..=self.rng.below(max)).map(|_| item(self)).collect())
    }

    /// Each of `names` as a switch, or left out.
    fn flags(&mut self, names: &[&str]) -> Value {
        let mut flags = Map::new();
        for name in names {
            if self.rng.chance(70) {
                flags.insert((*name).to_owned(), json!(self.rng.chance(50)));
            }
        }
        if flags.is_empty()
            && let Some(name) = names.first()
        {
            flags.insert((*name).to_owned(), json!(true));
        }
        Value::Object(flags)
    }
}

/// `config` as block YAML, every string double-quoted.
fn yaml(config: &Map<String, Value>) -> String {
    let mut out = String::new();
    write_mapping(&mut out, config, 0, None);
    out
}

/// A mapping's entries at `indent`; `lead` starts the first line in place
/// of the indent, for a mapping that is a sequence item.
fn write_mapping(
    out: &mut String,
    map: &Map<String, Value>,
    indent: usize,
    mut lead: Option<String>,
) {
    for (key, value) in map {
        match lead.take() {
            Some(lead) => out.push_str(&lead),
            None => out.push_str(&" ".repeat(indent)),
        }
        out.push_str(&scalar(&Value::from(key.as_str())));
        out.push(':');
        write_value(out, value, indent + 2);
    }
}

/// What follows a key or a sequence dash: a nested block, or a scalar on
/// the same line.
fn write_value(out: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            out.push('\n');
            write_mapping(out, map, indent, None);
        }
        Value::Array(items) if !items.is_empty() => {
            out.push('\n');
            for item in items {
                let dash = format!("{}- ", " ".repeat(indent));
                match item {
                    Value::Object(map) if !map.is_empty() => {
                        write_mapping(out, map, indent + 2, Some(dash));
                    }
                    Value::Array(items) if !items.is_empty() => {
                        out.push_str(dash.trim_end());
                        write_value(out, item, indent + 2);
                    }
                    _ => {
                        out.push_str(&dash);
                        out.push_str(&scalar(item));
                        out.push('\n');
                    }
                }
            }
        }
        _ => {
            out.push(' ');
            out.push_str(&scalar(value));
            out.push('\n');
        }
    }
}

/// A scalar as YAML: strings double-quoted with control characters
/// escaped, everything else as JSON writes it.
fn scalar(value: &Value) -> String {
    let Value::String(text) = value else {
        return value.to_string();
    };
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ch if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}' | '\u{feff}') => {
                out.push_str(&format!("\\u{:04x}", u32::from(ch)));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_nests_mappings_and_sequences() {
        let config = json!({
            "a": { "b": [ { "c": 1, "d": ["x", "y\"\n"] }, "plain" ] },
            "e": true,
        });
        let Value::Object(config) = config else {
            unreachable!()
        };
        assert_eq!(
            yaml(&config),
            "\"a\":\n  \"b\":\n    - \"c\": 1\n      \"d\":\n        - \"x\"\n        - \"y\\\"\\u000a\"\n    - \"plain\"\n\"e\": true\n"
        );
    }

    /// The generated configs parse, and most pairs differ in something the
    /// diff reports, so the suite compares lines rather than empty lists.
    #[test]
    fn cases_parse_and_mostly_change() {
        let cases = detail_cases(13, 300);
        let mut changed = 0;
        for case in &cases {
            let details = crate::config_diff::details(case).unwrap();
            let lines = details["details"].as_array();
            assert!(lines.is_some(), "{}: {details}", case.name);
            if lines.is_some_and(|lines| !lines.is_empty()) {
                changed += 1;
            }
        }
        assert!(
            changed * 10 >= cases.len() * 7,
            "{changed} of {}",
            cases.len()
        );
    }
}
