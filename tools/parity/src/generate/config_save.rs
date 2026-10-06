//! Seeded random config files and writes for the config writer.
//!
//! Each case is a config file and one to four writes to it. The file holds
//! a random mix of the settings open-ferry types (from
//! [`super::config_diff`]'s generator) and of the settings it doesn't type:
//! `plugins` (with plugin configs), `pprof`, `models`, `antigravity`,
//! `devin`, `claude-header-defaults`, a Claude key's `cloak` and
//! `fingerprint-profile`, and a section no version knows. A file is in the
//! legacy layout or, for the settings that move to one place, the v8 one.
//! It is written in a random style: indents of 2 or 4, indentless lists,
//! flow collections, plain, single- and double-quoted and block scalars,
//! anchors and aliases, head, line and foot comments, comments at the
//! wrong indent, blank lines, and document comments. A quarter of the
//! files are in the style the writer itself writes (block collections;
//! indent 2 with full-line comments at column 0, or indent 4 with them at
//! their entries in the v8 layout), so that what it keeps can be checked
//! byte for byte; each of those is first saved with a changed config.
//!
//! The writes save the file as it stands or a changed config, with and
//! without the move to the v8 layout; set a nested string, as `LoadConfig`
//! does with a management key's hash; or write a whole file, as the
//! management API does.
//!
//! What the generator avoids, as open-ferry deliberately differs there
//! (see UPSTREAM.md's "The config writer"):
//! - A save whose config holds other values for the untyped settings than
//!   the file does: open-ferry keeps the file's, upstream writes the
//!   config's. A save's config carries the file's untyped settings, as a
//!   management write's config (read from the file) does. Their values are
//!   spelt as upstream writes them (`true`, not `yes`) and hold no empty
//!   value upstream leaves out. (A save's `plugins` are the file's on
//!   upstream's side: see `go/parity_config_save.go`.)
//! - Plugin IDs out of order or quoted: upstream writes them from a map,
//!   sorted and plain (the hand-written `plugins-configs-rebuilt` case).
//! - A `plugins.dir` that upstream's `ResolvePluginsDir` changes (a path
//!   with a separator, which it cleans with the system's, or one starting
//!   with `~`): upstream writes the changed one (the hand-written
//!   `plugins-dir-cleaned` case).
//! - A Claude key's `cloak` or `fingerprint-profile` on a key the save
//!   adds, as open-ferry can't write what it doesn't type: a save's Claude
//!   key carries them exactly when it takes the place of a key of the file
//!   that has them.
//! - A line comment on an OAuth map's key: emptying the map then makes
//!   upstream write a file its own `LoadConfig` refuses (the hand-written
//!   `oauth-clear` case), which ends the case early. Other files upstream
//!   writes and its `LoadConfig` refuses still come up, and count as
//!   equivalent where open-ferry refuses the write (see
//!   `config_save::drop_unloadable`).
//! - A save of the file as it stands once a save may have left two OAuth
//!   channels in it that are the same once trimmed and in lower case (the
//!   writer keeps the file's channel and adds the config's): which one Go's
//!   loader keeps depends on its map order (UPSTREAM.md's "Maps iterate in
//!   key order"). Such a step saves a changed config instead.
//! - What [`super::config_diff`]'s generator avoids, and timestamps.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::config_diff::{Configs, yaml};
use super::{Generator, Rng};
use crate::cases::Case;

/// Keeps these files apart from the config change details' configs for the
/// same seed.
const SALT: u64 = 0x0C0F_F1E5_A7E0_0001;

/// Where the v8 layout puts the legacy settings the generator writes in it,
/// the untyped ones included. Other settings are left out of a v8 file.
const V8_MOVES: &[(&str, &str)] = &[
    ("port", "server.port"),
    ("auth-dir", "oauth.auth-dir"),
    ("debug", "observability.logs.debug"),
    ("logging-to-file", "observability.logs.logging-to-file"),
    (
        "usage-statistics-enabled",
        "observability.usage.usage-statistics-enabled",
    ),
    (
        "redis-usage-queue-retention-seconds",
        "observability.usage.redis-usage-queue-retention-seconds",
    ),
    ("disable-cooling", "routing.cooldown.disable-cooling"),
    (
        "save-cooldown-status",
        "routing.cooldown.save-cooldown-status",
    ),
    (
        "transient-error-cooldown-seconds",
        "routing.cooldown.transient-error-cooldown-seconds",
    ),
    (
        "disable-image-generation",
        "multimedia.disable-image-generation",
    ),
    ("request-log", "observability.logs.request-log"),
    (
        "logs-max-total-size-mb",
        "observability.logs.logs-max-total-size-mb",
    ),
    (
        "error-logs-max-files",
        "observability.logs.error-logs-max-files",
    ),
    ("request-retry", "routing.retry.request-retry"),
    (
        "max-retry-credentials",
        "routing.retry.max-retry-credentials",
    ),
    ("max-retry-interval", "routing.retry.max-retry-interval"),
    ("proxy-url", "requests.proxy-url"),
    ("force-model-prefix", "routing.force-model-prefix"),
    (
        "nonstream-keepalive-interval",
        "requests.nonstream-keepalive-interval",
    ),
    ("quota-exceeded", "quota-exceeded"),
    ("codex", "upstream.codex"),
    ("xai", "upstream.xai"),
    ("client", "client"),
    ("routing", "routing"),
    ("api-keys", "access.api-keys"),
    ("payload", "requests.payload"),
    ("oauth-excluded-models", "oauth.excluded-models"),
    ("oauth-model-alias", "oauth.model-alias"),
    ("oauth-request-scoped-errors", "oauth.request-scoped-errors"),
    ("oauth-settings", "oauth.settings"),
    ("remote-management", "management"),
    ("plugins", "plugins"),
    ("models", "models"),
    ("pprof", "observability.pprof"),
    ("antigravity", "oauth.providers.antigravity"),
    ("devin", "oauth.providers.devin"),
    ("claude-header-defaults", "upstream.claude.header-defaults"),
    ("x-extension", "x-extension"),
];

/// The keys whose mapping a save can empty: the OAuth maps, by their
/// legacy and v8 names, and the v8 section holding them.
const OAUTH_MAPS: &[&str] = &[
    "oauth-excluded-models",
    "oauth-model-alias",
    "oauth-request-scoped-errors",
    "oauth-settings",
    "excluded-models",
    "model-alias",
    "request-scoped-errors",
    "settings",
    "oauth",
];

/// bcrypt-shaped management key hashes, as `LoadConfig` sets one.
const HASHES: &[&str] = &[
    "$2a$10$abcdefghijklmnopqrstuv",
    "$2b$10$zyxwvutsrqponmlkjihgfe",
];

const PPROF_ADDRS: &[&str] = &["127.0.0.1:8316", "0.0.0.0:6060", "localhost:7070"];

const CATALOG_URLS: &[&str] = &[
    "https://catalog.example/models.json",
    "https://catalog.example/codex.json",
];

const CATALOGS: &[&str] = &[
    "",
    "https://catalog.example/models.json",
    "https://catalog.example/codex.json",
];

const WORDS: &[&str] = &["acme", "project-x", "internal name", "Orion"];

/// Case options for `config-save/steps`. A case written in the writer's
/// own style also lists the untyped sections of its file under `"kept"`.
pub fn step_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut writes = Writes {
                configs: Configs {
                    rng: Generator::new(seed ^ SALT, index).rng,
                },
                extras: BTreeMap::new(),
                names: 0,
            };
            Case::new(format!("random-{seed}-{index}"), "", "").with_options(writes.case())
        })
        .collect()
}

/// The random source for one case.
struct Writes {
    configs: Configs,
    /// The untyped fields of each Claude key, by its trimmed `api-key`,
    /// picked when the key is first seen, so that a save's key carries
    /// what the file's key it matches has.
    extras: BTreeMap<String, Option<Map<String, Value>>>,
    /// The comments and anchors named so far.
    names: usize,
}

/// What the file holds after the writes so far.
struct File {
    /// The typed settings, in the legacy layout, as the generator made them.
    typed: Map<String, Value>,
    /// The untyped sections, in the legacy layout.
    untyped: Map<String, Value>,
    /// The file's Claude keys, in order: each one's trimmed `api-key`, and
    /// whether it carries untyped fields.
    claude: Vec<(String, bool)>,
    v8: bool,
    /// Whether the file was written with an OAuth channel that isn't
    /// trimmed and in lower case, which the writer keeps.
    raw_channels: bool,
    /// Whether a save may have added a channel beside one of those that is
    /// the same once trimmed and in lower case.
    channels_collide: bool,
}

impl File {
    /// After a save: in the v8 layout when it migrated or the file was
    /// already in it, or upstream reads it so (see [`reads_as_v8`]).
    fn saved(&mut self, migrate: bool) {
        self.v8 |= migrate || reads_as_v8(&self.typed, &self.untyped);
        self.channels_collide |= self.raw_channels;
    }
}

/// Whether an OAuth map of `typed` has a channel that isn't trimmed and in
/// lower case.
fn raw_channels(typed: &Map<String, Value>) -> bool {
    OAUTH_MAPS
        .iter()
        .filter(|name| name.starts_with("oauth-"))
        .filter_map(|name| typed.get(*name)?.as_object())
        .flat_map(Map::keys)
        .any(|channel| channel.trim().to_lowercase() != *channel)
}

/// Whether upstream's `IsV8ConfigLayout` reads a legacy file with these
/// settings as v8: `models` is a v8 section, and
/// `client.codex.optimize-multi-agent-v2` is where v8 moves the legacy
/// `codex.optimize-multi-agent-v2`.
fn reads_as_v8(typed: &Map<String, Value>, untyped: &Map<String, Value>) -> bool {
    untyped.contains_key("models")
        || typed
            .get("client")
            .and_then(|client| client.get("codex"))
            .and_then(|codex| codex.get("optimize-multi-agent-v2"))
            .is_some()
}

impl Writes {
    fn rng(&mut self) -> &mut Rng {
        &mut self.configs.rng
    }

    /// `{"file": ..., "steps": [...]}`, with `"kept"` for a file in the
    /// writer's style.
    fn case(&mut self) -> Value {
        let canonical = self.rng().chance(25);
        let v8 = self.rng().chance(30);
        let (text, kept, mut file) = self.file(v8, canonical);
        let mut steps = Vec::new();
        if canonical {
            steps.push(self.save(&mut file, Some(false)));
        }
        for _ in 0..=self.rng().below(if canonical { 2 } else { 4 }) {
            let step = match self.rng().below(20) {
                0..=4 if file.channels_collide => self.save(&mut file, None),
                0..=4 => {
                    let migrate = self.rng().chance(35);
                    file.saved(migrate);
                    json!({ "op": "save", "migrate": migrate })
                }
                5..=12 => self.save(&mut file, None),
                13..=16 => self.nested(&mut file),
                _ => {
                    let v8 = self.rng().chance(40);
                    let (body, _, written) = self.file(v8, false);
                    file = written;
                    json!({ "op": "write", "body": body })
                }
            };
            steps.push(step);
        }
        let mut options = json!({ "file": text, "steps": steps });
        if canonical {
            options["kept"] = json!(kept);
        }
        options
    }

    /// A new file: its text, the text of each untyped section when it is
    /// in the writer's style, and what it holds.
    fn file(&mut self, v8: bool, canonical: bool) -> (String, Vec<String>, File) {
        let mut typed = self.configs.config();
        if v8 {
            typed.retain(|key, _| V8_MOVES.iter().any(|(old, _)| old == key));
            if let Some(Value::Object(quota)) = typed.get_mut("quota-exceeded") {
                quota.remove("antigravity-credits");
                if quota.is_empty() {
                    typed.remove("quota-exceeded");
                }
            }
            if typed.is_empty() {
                typed.insert("port".to_owned(), json!(8317));
            }
        } else if canonical {
            // Read as v8 (see `reads_as_v8`), the file would move when saved.
            if let Some(Value::Object(client)) = typed.get_mut("client") {
                if let Some(Value::Object(codex)) = client.get_mut("codex") {
                    codex.remove("optimize-multi-agent-v2");
                    if codex.is_empty() {
                        client.remove("codex");
                    }
                }
                if client.is_empty() {
                    typed.remove("client");
                }
            }
        }
        // A legacy file with `models` moves to the v8 layout when saved;
        // one in the writer's style doesn't get it, so that its sections
        // stay where they are.
        let models = match (v8, canonical) {
            (true, _) => 45,
            (false, true) => 0,
            (false, false) => 25,
        };
        let untyped = self.untyped(models);
        let mut tree = typed.clone();
        let claude = self.file_extras(&mut tree);
        let mut entries: Vec<(String, Value)> = tree.into_iter().chain(untyped.clone()).collect();
        self.rng().shuffle(&mut entries);
        let mut tree: Map<String, Value> = entries.into_iter().collect();
        if v8 {
            tree = self.v8_layout(tree);
        }
        let mut names = self.names;
        let mut style = Style::new(&mut self.configs.rng, &mut names, canonical, v8);
        let (text, sections) = style.render(&tree);
        self.names = names;
        // The untyped sections where they stay; a v8 file's unknown
        // sections are commented out.
        let paths: &[&str] = if v8 {
            &[
                "plugins",
                "models",
                "observability.pprof",
                "oauth.providers.antigravity",
                "oauth.providers.devin",
                "upstream.claude.header-defaults",
            ]
        } else {
            &[
                "plugins",
                "pprof",
                "antigravity",
                "devin",
                "claude-header-defaults",
                "x-extension",
            ]
        };
        let kept = sections
            .into_iter()
            .filter(|(path, _)| canonical && paths.contains(&path.as_str()))
            .map(|(_, text)| text)
            .collect();
        let v8 = v8 || reads_as_v8(&typed, &untyped);
        let file = File {
            raw_channels: raw_channels(&typed),
            typed,
            untyped,
            claude,
            v8,
            channels_collide: false,
        };
        (text, kept, file)
    }

    /// A save of a changed config over `file`, migrating it or not as
    /// `migrate` says, or at random.
    fn save(&mut self, file: &mut File, migrate: Option<bool>) -> Value {
        let migrate = match migrate {
            Some(migrate) => migrate,
            None => self.rng().chance(30),
        };
        let typed = self.configs.changed(&file.typed);
        let mut config = typed.clone();
        file.claude = self.save_extras(&mut config, &file.claude);
        for (key, value) in &file.untyped {
            config.insert(key.clone(), value.clone());
        }
        file.typed = typed;
        file.saved(migrate);
        json!({ "op": "save", "config": yaml(&config), "migrate": migrate })
    }

    /// A nested string set in `file`: a management key's hash, or an
    /// untyped setting.
    fn nested(&mut self, file: &mut File) -> Value {
        let v8 = file.v8;
        let (keys, value): (Vec<&str>, &str) = match self.rng().below(3) {
            0 => {
                let hash = self.rng().pick(HASHES);
                set(&mut file.typed, &["remote-management", "secret-key"], hash);
                let keys = if v8 {
                    vec!["management", "secret-key"]
                } else {
                    vec!["remote-management", "secret-key"]
                };
                (keys, hash)
            }
            1 => {
                let addr = self.rng().pick(PPROF_ADDRS);
                set(&mut file.untyped, &["pprof", "addr"], addr);
                let keys = if v8 {
                    vec!["observability", "pprof", "addr"]
                } else {
                    vec!["pprof", "addr"]
                };
                (keys, addr)
            }
            _ => {
                let catalog = self.rng().pick(CATALOG_URLS);
                set(&mut file.untyped, &["models", "catalog"], catalog);
                (vec!["models", "catalog"], catalog)
            }
        };
        json!({ "op": "nested", "keys": keys, "value": value })
    }

    /// The untyped sections of a new file, each set or not, `models` with
    /// the chance `models` in percent.
    fn untyped(&mut self, models: u64) -> Map<String, Value> {
        let rng = self.rng();
        let mut sections = Map::new();
        if rng.chance(45) {
            let mut plugins = Map::new();
            plugins.insert("enabled".to_owned(), json!(rng.chance(50)));
            if rng.chance(50) {
                plugins.insert(
                    "dir".to_owned(),
                    json!(rng.pick(&["plugins", "ferry-plugins"])),
                );
            }
            if rng.chance(30) {
                plugins.insert(
                    "store-sources".to_owned(),
                    json!(["https://store.example/index.json"]),
                );
            }
            if rng.chance(70) {
                let mut configs = Map::new();
                for id in ["alpha", "beta", "gamma"] {
                    if rng.chance(50) {
                        configs.insert(id.to_owned(), plugin(rng));
                    }
                }
                if configs.is_empty() {
                    configs.insert("alpha".to_owned(), json!({ "enabled": true }));
                }
                plugins.insert("configs".to_owned(), Value::Object(configs));
            }
            sections.insert("plugins".to_owned(), Value::Object(plugins));
        }
        if rng.chance(45) {
            let mut pprof = Map::new();
            pprof.insert("enable".to_owned(), json!(rng.chance(50)));
            if rng.chance(60) {
                pprof.insert("addr".to_owned(), json!(rng.pick(PPROF_ADDRS)));
            }
            sections.insert("pprof".to_owned(), Value::Object(pprof));
        }
        if rng.chance(models) {
            let mut models = Map::new();
            for key in ["catalog", "codex-catalog", "devin-catalog"] {
                if rng.chance(50) {
                    models.insert(key.to_owned(), json!(rng.pick(CATALOGS)));
                }
            }
            if models.is_empty() {
                models.insert(
                    "catalog".to_owned(),
                    json!("https://catalog.example/models.json"),
                );
            }
            sections.insert("models".to_owned(), Value::Object(models));
        }
        if rng.chance(35) {
            let mut antigravity = Map::new();
            if rng.chance(60) {
                antigravity.insert("sensitive-words".to_owned(), words(rng));
            }
            if antigravity.is_empty() || rng.chance(50) {
                let mut pool = Map::new();
                pool.insert("enabled".to_owned(), json!(rng.chance(50)));
                if rng.chance(50) {
                    pool.insert(
                        "idle-conn-timeout".to_owned(),
                        json!(rng.pick(&["90s", "2m"])),
                    );
                }
                if rng.chance(50) {
                    pool.insert(
                        "max-idle-conns-per-host".to_owned(),
                        json!(rng.pick(&[0, 4])),
                    );
                }
                antigravity.insert("connection-pool".to_owned(), Value::Object(pool));
            }
            sections.insert("antigravity".to_owned(), Value::Object(antigravity));
        }
        if rng.chance(35) {
            sections.insert("devin".to_owned(), json!({ "sensitive-words": words(rng) }));
        }
        if rng.chance(35) {
            let mut defaults = Map::new();
            for (key, value) in [
                ("user-agent", "example-agent/1.0"),
                ("package-version", "1.0.0"),
                ("runtime-version", "v22.0.0"),
                ("os", "linux"),
                ("arch", "x64"),
                ("timeout", "600"),
                ("timezone", "UTC"),
            ] {
                if rng.chance(40) {
                    defaults.insert(key.to_owned(), json!(value));
                }
            }
            if defaults.is_empty() || rng.chance(30) {
                defaults.insert("stabilize-device-profile".to_owned(), json!(rng.chance(50)));
            }
            sections.insert("claude-header-defaults".to_owned(), Value::Object(defaults));
        }
        if rng.chance(30) {
            sections.insert(
                "x-extension".to_owned(),
                json!({ "owner": "ops", "notes": ["first", "second note"], "limits": { "burst": 10 } }),
            );
        }
        sections
    }

    /// Gives the Claude keys of a new file their untyped fields, and lists
    /// the keys.
    fn file_extras(&mut self, config: &mut Map<String, Value>) -> Vec<(String, bool)> {
        let Some(Value::Array(entries)) = config.get_mut("claude-api-key") else {
            return Vec::new();
        };
        let mut claude = Vec::new();
        for entry in entries {
            let key = api_key(entry);
            let extra = self.extra(&key);
            if let (Some(extra), Value::Object(entry)) = (&extra, &mut *entry) {
                entry.extend(extra.clone());
            }
            claude.push((key, extra.is_some()));
        }
        claude
    }

    /// Gives the Claude keys of a save's config the untyped fields of the
    /// file's keys they take the place of (the first one not yet taken with
    /// the same `api-key`, as the writer matches list items), and lists the
    /// keys the file then has.
    fn save_extras(
        &mut self,
        config: &mut Map<String, Value>,
        file: &[(String, bool)],
    ) -> Vec<(String, bool)> {
        let Some(Value::Array(entries)) = config.get_mut("claude-api-key") else {
            return Vec::new();
        };
        let mut taken = vec![false; file.len()];
        let mut claude = Vec::new();
        for entry in entries {
            let key = api_key(entry);
            let matched = file
                .iter()
                .zip(&mut taken)
                .find(|((other, _), taken)| !**taken && *other == key)
                .map(|((_, has), taken)| {
                    *taken = true;
                    *has
                });
            let has = matched.unwrap_or(false);
            if has
                && let (Some(Some(extra)), Value::Object(entry)) =
                    (self.extras.get(&key), &mut *entry)
            {
                entry.extend(extra.clone());
            }
            claude.push((key, has));
        }
        claude
    }

    /// The untyped fields of the Claude key `key`, picked when first asked.
    fn extra(&mut self, key: &str) -> Option<Map<String, Value>> {
        if let Some(extra) = self.extras.get(key) {
            return extra.clone();
        }
        let rng = self.rng();
        let extra = rng.chance(60).then(|| {
            let mut extra = Map::new();
            if rng.chance(80) {
                let mut cloak = Map::new();
                cloak.insert(
                    "mode".to_owned(),
                    json!(rng.pick(&["auto", "always", "never"])),
                );
                if rng.chance(40) {
                    cloak.insert("strict-mode".to_owned(), json!(true));
                }
                if rng.chance(50) {
                    cloak.insert("sensitive-words".to_owned(), words(rng));
                }
                if rng.chance(50) {
                    cloak.insert("cache-user-id".to_owned(), json!(rng.chance(50)));
                }
                extra.insert("cloak".to_owned(), Value::Object(cloak));
            }
            if extra.is_empty() || rng.chance(50) {
                extra.insert("fingerprint-profile".to_owned(), json!("claude-code-cli"));
            }
            extra
        });
        self.extras.insert(key.to_owned(), extra.clone());
        extra
    }

    /// `config` in the v8 layout, `config-version` first or last.
    fn v8_layout(&mut self, config: Map<String, Value>) -> Map<String, Value> {
        let last = self.rng().chance(20);
        let mut v8 = Map::new();
        if !last {
            v8.insert("config-version".to_owned(), json!(8));
        }
        for (key, value) in config {
            if let Some((_, path)) = V8_MOVES.iter().find(|(old, _)| *old == key) {
                merge_at(&mut v8, path, value);
            }
        }
        if last {
            v8.insert("config-version".to_owned(), json!(8));
        }
        v8
    }
}

/// A plugin's config: upstream keeps it as the file has it.
fn plugin(rng: &mut Rng) -> Value {
    let mut config = Map::new();
    if rng.chance(70) {
        config.insert("enabled".to_owned(), json!(rng.chance(50)));
    }
    if rng.chance(40) {
        config.insert("priority".to_owned(), json!(rng.pick(&[1, 10, -5])));
    }
    if rng.chance(60) {
        let mut settings = Map::new();
        settings.insert(
            "endpoint".to_owned(),
            json!(rng.pick(&["https://hook.example/run", "unix:///run/plugin.sock"])),
        );
        if rng.chance(50) {
            settings.insert("retries".to_owned(), json!(rng.pick(&[0, 3])));
        }
        if rng.chance(50) {
            settings.insert("tags".to_owned(), json!(["a", "b c"]));
        }
        if rng.chance(30) {
            settings.insert("nested".to_owned(), json!({ "deep": { "on": true } }));
        }
        config.insert("settings".to_owned(), Value::Object(settings));
    }
    if config.is_empty() {
        config.insert("enabled".to_owned(), json!(true));
    }
    Value::Object(config)
}

/// One to three distinct sensitive words.
fn words(rng: &mut Rng) -> Value {
    let mut words: Vec<&str> = WORDS.to_vec();
    rng.shuffle(&mut words);
    words.truncate(1 + rng.below(3));
    json!(words)
}

/// An entry's `api-key`, trimmed, as the writer matches list items by it.
fn api_key(entry: &Value) -> String {
    entry["api-key"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Sets the string at `keys` in `config`, making mappings on the way.
fn set(config: &mut Map<String, Value>, keys: &[&str], value: &str) {
    let Some((last, parents)) = keys.split_last() else {
        return;
    };
    let mut map = config;
    for key in parents {
        let entry = map.entry((*key).to_owned()).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        let Value::Object(next) = entry else {
            return;
        };
        map = next;
    }
    map.insert((*last).to_owned(), json!(value));
}

/// Puts `value` at the dotted `path` of `config`, merging a mapping into
/// one already there.
fn merge_at(config: &mut Map<String, Value>, path: &str, value: Value) {
    match path.split_once('.') {
        Some((head, rest)) => {
            let entry = config.entry(head.to_owned()).or_insert_with(|| json!({}));
            if let Value::Object(child) = entry {
                merge_at(child, rest, value);
            }
        }
        None => match (config.get_mut(path), value) {
            (Some(Value::Object(existing)), Value::Object(new)) => existing.extend(new),
            (_, value) => {
                config.insert(path.to_owned(), value);
            }
        },
    }
}

/// Writes a config tree as YAML in a random style.
struct Style<'a> {
    rng: &'a mut Rng,
    /// The comments and anchors named so far, for unique names.
    names: &'a mut usize,
    /// In the writer's own style: its indent, block collections, no anchors,
    /// block scalars, blank lines or foot comments (which yaml.v3 moves),
    /// and head and line comments where they belong.
    canonical: bool,
    /// Full-line comments at column 0, as upstream's
    /// `NormalizeCommentIndentation` leaves a legacy file.
    flush_comments: bool,
    step: usize,
    indentless: bool,
    /// The chance, in percent, of a collection in flow style.
    flow: u64,
    /// The chance, in percent, of a comment in each place.
    comments: u64,
    /// The values anchored so far (as JSON), with their anchor names.
    anchors: Vec<(String, String)>,
    /// In the `plugins` section, whose plugin IDs are plain.
    plugins: bool,
    /// The text of each mapping entry outside lists, with its comments, by
    /// its dotted path.
    sections: Vec<(String, String)>,
}

impl<'a> Style<'a> {
    /// A style; one in the writer's own style has its indent and comment
    /// placement: 2 with comments at column 0, or 4 with comments at their
    /// entries in the v8 layout, which upstream writes last with
    /// `yaml.Marshal`.
    fn new(rng: &'a mut Rng, names: &'a mut usize, canonical: bool, v8: bool) -> Self {
        let (step, indentless, flow, comments) = if canonical {
            (if v8 { 4 } else { 2 }, false, 0, 40)
        } else {
            (
                rng.pick(&[2, 2, 4]),
                rng.chance(30),
                rng.pick(&[0, 10, 25]),
                rng.pick(&[10, 30, 60]),
            )
        };
        Self {
            rng,
            names,
            canonical,
            flush_comments: canonical && !v8,
            step,
            indentless,
            flow,
            comments,
            anchors: Vec::new(),
            plugins: false,
            sections: Vec::new(),
        }
    }

    /// The document, and the text of each mapping entry outside lists with
    /// its comments, by its dotted path.
    fn render(&mut self, root: &Map<String, Value>) -> (String, Vec<(String, String)>) {
        let mut out = String::new();
        if !self.canonical && self.rng.chance(15) {
            out.push_str("---\n");
        }
        if !self.canonical && self.rng.chance(25) {
            let comment = self.comment();
            out.push_str(&comment);
            out.push_str("\n\n");
        }
        for (index, (key, value)) in root.iter().enumerate() {
            if index > 0 && !self.canonical && self.rng.chance(30) {
                out.push('\n');
            }
            self.entry(&mut out, Some(""), key, value, 0, None);
        }
        if !self.canonical && self.rng.chance(25) {
            let comment = self.comment();
            out.push('\n');
            out.push_str(&comment);
            out.push('\n');
        }
        (out, std::mem::take(&mut self.sections))
    }

    /// One `key: value` entry at `indent`; `lead` starts its line in place
    /// of the indent, for the first entry of a mapping in a list. An entry
    /// outside lists has the dotted path of its mapping, and its text is
    /// recorded.
    fn entry(
        &mut self,
        out: &mut String,
        parent: Option<&str>,
        key: &str,
        value: &Value,
        indent: usize,
        lead: Option<&str>,
    ) {
        let start = out.len();
        let path = parent.map(|parent| {
            if parent.is_empty() {
                key.to_owned()
            } else {
                format!("{parent}.{key}")
            }
        });
        let plugins = self.plugins;
        self.plugins |= path.as_deref() == Some("plugins");
        self.entry_text(out, path.as_deref(), key, value, indent, lead);
        self.plugins = plugins;
        if let Some(path) = path {
            let text = out.get(start..).unwrap_or_default().to_owned();
            self.sections.push((path, text));
        }
    }

    fn entry_text(
        &mut self,
        out: &mut String,
        path: Option<&str>,
        key: &str,
        value: &Value,
        indent: usize,
        lead: Option<&str>,
    ) {
        match lead {
            Some(lead) => out.push_str(lead),
            None => {
                self.head_comment(out, indent);
                out.push_str(&" ".repeat(indent));
            }
        }
        let key_text = self.key(key);
        out.push_str(&key_text);
        out.push(':');
        let child = indent + self.step;
        if let Some(alias) = self.alias(value) {
            out.push_str(" *");
            out.push_str(&alias);
            self.line_comment(out, key);
            out.push('\n');
            return;
        }
        match value {
            Value::Object(map) if !map.is_empty() => {
                if self.rng.chance(self.flow) {
                    out.push(' ');
                    self.flow_value(out, value);
                    self.line_comment(out, key);
                    out.push('\n');
                    return;
                }
                self.anchor(out, value);
                self.line_comment(out, key);
                out.push('\n');
                for (key, value) in map {
                    self.entry(out, path, key, value, child, None);
                }
                self.foot_comment(out, child);
            }
            Value::Array(items) if !items.is_empty() => {
                if self.rng.chance(self.flow) {
                    out.push(' ');
                    self.flow_value(out, value);
                    self.line_comment(out, key);
                    out.push('\n');
                    return;
                }
                self.anchor(out, value);
                self.line_comment(out, key);
                out.push('\n');
                let at = if self.indentless { indent } else { child };
                self.sequence(out, items, at);
            }
            _ => {
                out.push(' ');
                self.scalar_line(out, key, value, child);
            }
        }
    }

    /// A block list's items, their dashes at `indent`.
    fn sequence(&mut self, out: &mut String, items: &[Value], indent: usize) {
        for item in items {
            self.head_comment(out, indent);
            let dash = format!("{}- ", " ".repeat(indent));
            match item {
                Value::Object(map) if !map.is_empty() && !self.rng.chance(self.flow) => {
                    for (index, (key, value)) in map.iter().enumerate() {
                        let lead = (index == 0).then_some(dash.as_str());
                        self.entry(out, None, key, value, indent + 2, lead);
                    }
                }
                Value::Object(_) | Value::Array(_) => {
                    out.push_str(&dash);
                    self.flow_value(out, item);
                    self.line_comment(out, "");
                    out.push('\n');
                }
                _ => {
                    out.push_str(&dash);
                    self.scalar_line(out, "", item, indent + 2);
                }
            }
        }
    }

    /// A scalar after its key or dash, to the end of its line: an alias, a
    /// block scalar whose text is indented to `child`, or a scalar.
    fn scalar_line(&mut self, out: &mut String, key: &str, value: &Value, child: usize) {
        if let Some(alias) = self.alias(value) {
            out.push('*');
            out.push_str(&alias);
            self.line_comment(out, key);
            out.push('\n');
            return;
        }
        if let Value::String(text) = value
            && !self.canonical
            && block_ok(text)
            && self.rng.chance(8)
        {
            out.push_str(if self.rng.chance(50) { "|-" } else { ">-" });
            self.line_comment(out, key);
            out.push('\n');
            out.push_str(&" ".repeat(child));
            out.push_str(text);
            out.push('\n');
            return;
        }
        if !self.canonical && self.rng.chance(6) {
            let name = self.name("s");
            out.push('&');
            out.push_str(&name);
            out.push(' ');
            self.anchors.push((value.to_string(), name));
        }
        let text = self.scalar(value, false);
        out.push_str(&text);
        self.line_comment(out, key);
        out.push('\n');
    }

    /// A collection in flow style.
    fn flow_value(&mut self, out: &mut String, value: &Value) {
        match value {
            Value::Object(map) => {
                out.push('{');
                for (index, (key, value)) in map.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    let key = self.key(key);
                    out.push_str(&key);
                    out.push_str(": ");
                    self.flow_value(out, value);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    self.flow_value(out, item);
                }
                out.push(']');
            }
            _ => {
                let text = self.scalar(value, true);
                out.push_str(&text);
            }
        }
    }

    /// A mapping key: plain where it can be, or quoted.
    fn key(&mut self, key: &str) -> String {
        // A plugin ID, in a block `plugins.configs` or a flow `plugins`.
        let id = self.plugins && matches!(key, "alpha" | "beta" | "gamma");
        if plain_ok(key, true) && (self.canonical || id || self.rng.chance(85)) {
            key.to_owned()
        } else if single_ok(key) && self.rng.chance(50) {
            single_quoted(key)
        } else {
            double_quoted(key)
        }
    }

    /// A scalar: a string plain, single- or double-quoted, and anything
    /// else as JSON writes it.
    fn scalar(&mut self, value: &Value, flow: bool) -> String {
        let Value::String(text) = value else {
            return value.to_string();
        };
        match self.rng.below(10) {
            0..=4 if plain_ok(text, flow) => text.clone(),
            0..=6 if single_ok(text) => single_quoted(text),
            _ => double_quoted(text),
        }
    }

    /// An alias for `value`, sometimes, when it was anchored before.
    fn alias(&mut self, value: &Value) -> Option<String> {
        if self.anchors.is_empty() {
            return None;
        }
        let json = value.to_string();
        let name = self
            .anchors
            .iter()
            .find(|(anchored, _)| *anchored == json)
            .map(|(_, name)| name.clone())?;
        self.rng.chance(50).then_some(name)
    }

    /// Anchors a collection, sometimes.
    fn anchor(&mut self, out: &mut String, value: &Value) {
        if self.canonical || !self.rng.chance(8) {
            return;
        }
        let name = self.name("m");
        out.push_str(" &");
        out.push_str(&name);
        self.anchors.push((value.to_string(), name));
    }

    fn head_comment(&mut self, out: &mut String, indent: usize) {
        if !self.rng.chance(self.comments) {
            return;
        }
        let lines = if !self.canonical && self.rng.chance(20) {
            2
        } else {
            1
        };
        for _ in 0..lines {
            let left = !self.canonical && self.rng.chance(15);
            let at = if self.flush_comments || left {
                0
            } else {
                indent
            };
            let comment = self.comment();
            out.push_str(&" ".repeat(at));
            out.push_str(&comment);
            out.push('\n');
        }
    }

    /// A comment at the end of a line, but not on an OAuth map's key.
    fn line_comment(&mut self, out: &mut String, key: &str) {
        if OAUTH_MAPS.contains(&key) || !self.rng.chance(self.comments) {
            return;
        }
        let comment = self.comment();
        out.push(' ');
        out.push_str(&comment);
    }

    /// A comment after a block mapping's entries.
    fn foot_comment(&mut self, out: &mut String, indent: usize) {
        if self.canonical || !self.rng.chance(self.comments / 3) {
            return;
        }
        let comment = self.comment();
        out.push_str(&" ".repeat(indent));
        out.push_str(&comment);
        out.push('\n');
    }

    fn comment(&mut self) -> String {
        let name = self.name("c");
        match self.rng.below(5) {
            0 if !self.canonical => format!("#{name}"),
            1 if !self.canonical => format!("#   {name} with words"),
            _ => format!("# {name}"),
        }
    }

    fn name(&mut self, prefix: &str) -> String {
        *self.names += 1;
        format!("{prefix}{}", self.names)
    }
}

/// Whether `text` can be written plain and read back as the same string:
/// ASCII letters, digits and a few marks, starting with a letter or `/`,
/// and not a word YAML reads as a switch or null.
fn plain_ok(text: &str, flow: bool) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '/') {
        return false;
    }
    let marks: &[char] = if flow {
        &['-', '_', '.', '/', '@', '+']
    } else {
        &['-', '_', '.', '/', '@', '+', ':']
    };
    if !text
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || marks.contains(&ch))
        || text.ends_with(':')
    {
        return false;
    }
    !matches!(
        text.to_ascii_lowercase().as_str(),
        "y" | "n" | "yes" | "no" | "on" | "off" | "true" | "false" | "null"
    )
}

/// Whether `text` can be single-quoted: no control characters or line
/// separators.
fn single_ok(text: &str) -> bool {
    !text
        .chars()
        .any(|ch| ch.is_control() || matches!(u32::from(ch), 0x85 | 0x2028 | 0x2029 | 0xfeff))
}

/// Whether `text` can be a one-line block scalar: not empty, with no space
/// at either end and nothing [`single_ok`] refuses.
fn block_ok(text: &str) -> bool {
    !text.is_empty() && text.trim() == text && single_ok(text)
}

fn single_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `text` double-quoted, with `"`, the backslash and control characters
/// escaped.
fn double_quoted(text: &str) -> String {
    let backslash = char::from(92u8);
    let mut out = String::from("\"");
    for ch in text.chars() {
        if ch == '"' || ch == backslash {
            out.push(backslash);
            out.push(ch);
        } else if !single_ok(&ch.to_string()) {
            out.push(backslash);
            out.push_str(&format!("U{:08x}", u32::from(ch)));
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not upstream's: the styles parse to the values they write.
    #[test]
    fn styles_read_back() {
        assert!(plain_ok("gpt-6-sol", false));
        assert!(plain_ok("http://a.example:1/x", false));
        assert!(!plain_ok("http://a.example:1/x", true));
        assert!(!plain_ok("Yes", false));
        assert!(!plain_ok("1.0.0", false));
        assert!(!plain_ok(" padded", false));
        assert_eq!(single_quoted("it's"), "'it''s'");
        let backslash = char::from(92u8);
        assert_eq!(
            double_quoted(&format!("a\"{backslash}{}", char::from(1u8))),
            format!("\"a{backslash}\"{backslash}{backslash}{backslash}U00000001\"")
        );
    }

    /// Not upstream's: the generated files load, most writes succeed, and
    /// every kind of write and file comes up.
    #[test]
    fn cases_load_and_mostly_write() {
        let cases = step_cases(13, 200);
        let mut refused = 0;
        let mut ops = BTreeMap::new();
        let mut v8 = 0;
        let mut canonical = 0;
        for case in &cases {
            let file = case.options["file"].as_str().unwrap();
            assert!(
                open_ferry_core::config::Config::parse(file).is_ok(),
                "{}: {file}",
                case.name
            );
            if file.contains("config-version: 8") {
                v8 += 1;
            }
            if case.options.get("kept").is_some() {
                canonical += 1;
            }
            for step in case.options["steps"].as_array().unwrap() {
                *ops.entry(step["op"].as_str().unwrap().to_owned())
                    .or_insert(0) += 1;
            }
            let result = crate::config_save::steps(case).unwrap();
            if let Some(error) = result.get("error") {
                assert!(
                    !error.as_str().unwrap().starts_with("config: "),
                    "{}: {error}",
                    case.name
                );
                refused += 1;
            }
        }
        assert!(refused * 20 <= cases.len(), "{refused} refused");
        assert!(
            v8 >= 20 && canonical >= 20,
            "{v8} v8, {canonical} canonical"
        );
        assert_eq!(ops.len(), 3, "{ops:?}");
    }

    /// Not upstream's: once a save may have left two OAuth channels that
    /// are the same once trimmed and in lower case, the file isn't saved as
    /// it stands (seed 1's case 4999 did, after saving a file with
    /// `' claude '`).
    #[test]
    fn colliding_channels_are_not_resaved() {
        let typed = |channel: &str| {
            let mut typed = Map::new();
            typed.insert("oauth-model-alias".to_owned(), json!({ channel: [] }));
            typed
        };
        assert!(raw_channels(&typed(" claude ")));
        assert!(raw_channels(&typed("Claude")));
        assert!(!raw_channels(&typed("claude")));
        let cases = step_cases(1, 5000);
        let case = cases.last().unwrap();
        let file = case.options["file"].as_str().unwrap();
        assert!(file.contains("' claude ':"), "{file}");
        let steps = case.options["steps"].as_array().unwrap();
        let first_save = steps.iter().position(|step| step["op"] == "save");
        let resaved = steps.iter().enumerate().any(|(index, step)| {
            Some(index) > first_save && step["op"] == "save" && step.get("config").is_none()
        });
        assert!(first_save.is_some() && !resaved, "{steps:?}");
    }

    /// Not upstream's: the untyped sections of a file in the writer's own
    /// style come through a save of a changed config byte for byte, with
    /// their comments.
    #[test]
    fn untyped_sections_are_kept_byte_for_byte() {
        let mut checked = 0;
        for case in step_cases(13, 200) {
            let Some(kept) = case.options.get("kept").and_then(Value::as_array) else {
                continue;
            };
            let result = crate::config_save::steps(&case).unwrap();
            let Some(saved) = result["files"].get(0).and_then(Value::as_str) else {
                continue;
            };
            for section in kept {
                let section = section.as_str().unwrap();
                assert!(
                    saved.contains(section),
                    "{}: lost\n{section}\nfrom\n{saved}",
                    case.name
                );
                checked += 1;
            }
        }
        assert!(checked >= 30, "{checked}");
    }
}
