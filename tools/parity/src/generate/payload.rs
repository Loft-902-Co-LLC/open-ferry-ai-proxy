//! Seeded random cases for the config's payload rules: configs with
//! defaults, overrides and filters whose model names, protocols, headers
//! and conditions do and don't match the call; paths through a body's
//! objects and arrays with escaped dots, indexes, appends, wildcards, `#`
//! projections and `#(query)` keys, under a root or not; values of every
//! kind of YAML scalar, sequence and mapping, and raw JSON; clients'
//! requests that do and don't hold what a default would write;
//! `disable-image-generation` with image tools and `tool_choice` in each
//! form; tracked paths; and Codex clients' tools for the integer pass, flat
//! and in namespaces, with whole-number parameters at the top of a schema
//! and inside objects, array items and union branches.
//!
//! Upstream applies a rule's params in Go's random map order, so each
//! rule's params start with distinct plain keys. The cases stay clear of
//! what the payload module documents as a deviation: NaN and infinite
//! floats, a projection inside another, a count or wildcard under a
//! projection written to, `{` or `[` in a body's strings, and gjson syntax
//! it doesn't read.

use serde_json::{Map, Value, json};

use super::{Generator, num};
use crate::cases::Case;

const MODELS: &[&str] = &[
    "gpt-5",
    "gpt-5",
    "gpt-5-codex",
    "claude-opus-5",
    "gemini-2.5-pro",
    "Model-É",
    " gpt-5 ",
    "",
];

const REQUESTED: &[&str] = &[
    "",
    "",
    "",
    "alias",
    "alias(high)",
    "gpt-5(low)",
    " gpt-5(8192) ",
    "GPT-5",
    "claude-opus-5(none)",
];

const PROTOCOLS: &[&str] = &[
    "openai",
    "openai",
    "openai-response",
    "claude",
    "gemini",
    "codex",
    "gemini-cli",
    "antigravity",
    "",
];

const FROM: &[&str] = &[
    "",
    "openai",
    "openai-response",
    "claude",
    "gemini",
    "codex",
    "responses",
    "chat",
    "Anthropic",
];

const RULE_PROTOCOLS: &[&str] = &[
    "openai",
    "openai-response",
    "claude",
    "codex",
    "gemini",
    "OpenAI",
    " claude ",
];

const RULE_FROM: &[&str] = &[
    "openai",
    "responses",
    "chat",
    "claude",
    "anthropic",
    "gemini",
    "codex",
    "OpenAI-Response",
    "openai-responses",
];

const EXECUTORS: &[&str] = &[
    "",
    "",
    "claude",
    "gemini",
    "codex",
    "codex-websockets",
    "openai-compatibility",
    "xai",
    " Codex ",
];

const HEADER_NAMES: &[&str] = &["X-Tier", "X-Team", "x-tier", "X-Region"];

const HEADER_VALUES: &[&str] = &[
    "alpha",
    "Alpha",
    "beta-1",
    "tenant-a-region-us",
    "",
    " gamma ",
    "a",
];

const HEADER_PATTERNS: &[&str] = &[
    "alpha",
    "a*",
    "*",
    "ALPHA",
    "beta-?",
    "tenant-*-region-*",
    "gamma",
    "*a",
];

const USER_AGENTS: &[&str] = &[
    "codex_cli_rs/0.1.0",
    "codex-tui/0.154.0",
    "Codex Desktop/0.146.0",
    "curl/8.7.1",
    "",
];

const REQUEST_PATHS: &[&str] = &[
    "",
    "/v1/responses",
    "/v1/chat/completions",
    "/v1/images/generations",
    "/v1/images/edits",
    " /v1/images/edits ",
];

const IMAGE_MODES: &[&str] = &["true", "false", "chat", "passthrough"];

/// Codex's tools with the paths of their whole-number parameters, as the
/// integer pass knows them, then names it doesn't know.
const CODEX_TOOLS: &[(&str, &[&str])] = &[
    (
        "exec_command",
        &["yield_time_ms", "max_output_tokens", "timeout_ms"],
    ),
    (
        "write_stdin",
        &["session_id", "yield_time_ms", "max_output_tokens"],
    ),
    ("sleep", &["duration_ms"]),
    ("wait_agent", &["timeout_ms"]),
    ("wait", &["yield_time_ms", "max_tokens"]),
    ("tool_search", &["limit"]),
    (
        "test_sync_tool",
        &[
            "sleep_before_ms",
            "participants",
            "barrier.properties.participants",
            "barrier.properties.timeout_ms",
        ],
    ),
    ("create_goal", &["token_budget"]),
    ("get_channels", &["limit"]),
    ("list_threads", &["limit", "max_chars_per_post"]),
    ("search_posts", &["limit", "max_chars_per_post"]),
    ("read_thread", &["limit", "max_chars_per_post"]),
    ("read_post", &["offset_chars", "limit_chars"]),
    ("memories__list", &["max_results"]),
    ("memories__read", &["line_offset", "max_lines"]),
    ("memories__search", &["context_lines", "max_results"]),
    ("history__list_windows", &["limit"]),
    ("history__list_items", &["limit", "max_chars_per_item"]),
    ("history__read_item", &["offset_chars", "limit_chars"]),
    ("history__search_contents", &["limit"]),
    ("notes__list_files_by_prefix", &["max_results"]),
    (
        "notes__read_file",
        &[
            "start_line",
            "stop_line",
            "start_line.anyOf.0",
            "stop_line.anyOf.0",
        ],
    ),
    (
        "notes__search_contents",
        &["max_matches_per_file", "max_files"],
    ),
    ("image_gen__imagegen", &["num_last_images_to_include"]),
    (
        "web__run",
        &[
            "search_query.items.properties.recency",
            "image_query.items.properties.recency",
            "open.items.properties.lineno",
            "click.items.properties.id",
            "screenshot.items.properties.pageno",
            "weather.items.properties.duration",
            "sports.items.properties.num_games",
        ],
    ),
    ("collaboration__wait_agent", &["timeout_ms"]),
    ("multi_agent_v1__wait_agent", &["timeout_ms"]),
    ("collaboration__read_post", &["offset_chars", "limit_chars"]),
    (
        "collaboration__list_threads",
        &["limit", "max_chars_per_post"],
    ),
    ("functions__create_goal", &["token_budget"]),
    ("collab__exec_command", &["timeout_ms"]),
    (" sleep ", &["duration_ms"]),
    ("mcp__server__read_post", &["offset_chars"]),
    ("user_tools__read_post", &["limit_chars"]),
    ("multi_agent_v1__read_post", &["offset_chars"]),
    ("read", &["line_offset"]),
    ("run", &["open.items.properties.lineno"]),
    ("unknown", &["timeout_ms", "limit"]),
];

/// Parameter paths of no tool, or of another tool than the one drawn, or
/// that stop short of or go past a whole-number parameter.
const CODEX_OTHER_FIELDS: &[&str] = &[
    "other",
    "limit",
    "timeout_ms",
    "barrier.properties.ratio",
    "barrier.participants",
    "start_line.anyOf.1",
    "open.items.0.properties.lineno",
    "open.items.properties.lineno.items",
    "search_query.items.properties.recency",
];

/// The `type`s a parameter is declared with.
const CODEX_TYPES: &[&str] = &[
    r#""number""#,
    r#""number""#,
    r#""number""#,
    r#"["number","null"]"#,
    r#"["null","number","number","integer"]"#,
    r#""integer""#,
    r#""string""#,
];

/// Object keys in bodies and paths. Only `b.c` needs an escape in a path.
const KEYS: &[&str] = &[
    "a",
    "b",
    "c",
    "n",
    "s",
    "k",
    "v",
    "type",
    "name",
    "meta",
    "items",
    "list",
    "Mixed",
    "é",
    "sp ace",
    "b.c",
    "0",
    "under_score",
    "dash-key",
];

/// Fields of the objects in arrays, for queries to test.
const ITEM_KEYS: &[&str] = &["k", "v", "type", "name"];

const STRINGS: &[&str] = &[
    "x",
    "y",
    "z",
    "x",
    "hello",
    "",
    " padded ",
    "Ünïcødé",
    "🚀",
    "line\nbreak",
    "tab\there",
    "quote\"s",
    "back\\slash",
    "1",
    "true",
    "null",
    "<&>",
    "x*",
    "a.b",
];

const QUERY_STRINGS: &[&str] = &["x", "y", "z", "hello", "function"];

const QUERY_PATTERNS: &[&str] = &["x*", "*y", "h?llo", "*", "z"];

const NUMBERS: &[&str] = &[
    "0",
    "1",
    "2",
    "-3",
    "1.5",
    "2.50",
    "1e2",
    "0.1",
    "-0",
    "12345678901",
    "1.0",
];

const QUERY_NUMBERS: &[&str] = &["0", "1", "1.5", "2", "2.50", "-3"];

/// YAML integers, each of which upstream and serde read as the same number.
const YAML_INTS: &[&str] = &[
    "0",
    "1",
    "-1",
    "42",
    "+5",
    "9223372036854775807",
    "-9223372036854775808",
    "18446744073709551615",
    "0x1F",
    "0o17",
];

/// YAML floats, none of them NaN or infinite.
const YAML_FLOATS: &[&str] = &[
    "0.5",
    "1.0",
    "-2.5",
    "1e3",
    "1.5e-7",
    "1e21",
    "123456789.125",
    "0.1",
    "3.0e+2",
    "-0.0",
    "2.5E+10",
    "100000000000000000000.0",
];

/// Plain YAML scalars that are strings.
const YAML_WORDS: &[&str] = &["word", "gpt-5", "yes", "on", "2001-12-14", "x y", "é"];

/// Keys of YAML mappings, in no order.
const YAML_KEYS: &[&str] = &["z", "a", "m", "k", "é", "\"q k\""];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    /// A default's or override's path: no projection inside another, and
    /// none at the end, where it would be a count.
    Write,
    /// A filter's path.
    Filter,
    /// A condition's path.
    Read,
}

/// A path drawn from a body.
struct Path<'d> {
    /// Its first key.
    first: String,
    /// Whether the body has the first key, other than one the image strip
    /// may remove.
    first_held: bool,
    text: String,
    /// The body's value there, if the path follows what the body holds.
    at: Option<&'d Value>,
}

/// What a case's call holds, for its rules to match or not.
struct Call {
    model: &'static str,
    requested: &'static str,
    protocol: &'static str,
    from: &'static str,
    headers: Vec<(&'static str, &'static str)>,
}

/// `count` random cases for `payload/apply`, each depending only on `seed` and
/// its index.
pub fn apply_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut draw = Draw(Generator::new(seed, index));
            let (model, request, options) = draw.case();
            Case::new(format!("random-{seed}-{index}"), model, request).with_options(options)
        })
        .collect()
}

struct Draw(Generator);

impl Draw {
    fn below(&mut self, n: usize) -> usize {
        self.0.rng.below(n)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.0.rng.chance(percent)
    }

    fn pick(&mut self, items: &[&'static str]) -> &'static str {
        self.0.rng.pick(items)
    }

    fn case(&mut self) -> (&'static str, String, Value) {
        let mut headers = Vec::new();
        for _ in 0..self.below(4) {
            headers.push((self.pick(HEADER_NAMES), self.pick(HEADER_VALUES)));
        }
        if self.chance(25) {
            headers.push(("User-Agent", self.pick(USER_AGENTS)));
        }
        let call = Call {
            model: self.pick(MODELS),
            requested: self.pick(REQUESTED),
            protocol: self.pick(PROTOCOLS),
            from: self.pick(FROM),
            headers,
        };
        let root = if self.chance(20) {
            self.pick(&["request", " request "])
        } else {
            ""
        };
        let mut inner = self.body(0);
        if self.chance(25) {
            self.image_tools(&mut inner);
        }
        if self.chance(15) {
            self.codex_tools(&mut inner);
        }
        if self.chance(30) {
            inner.insert("model".into(), call.model.into());
        }
        let inner = Value::Object(inner);
        let body = self.under_root(root, inner.clone());
        let original = match self.below(10) {
            0..=4 => Value::Null,
            5 | 6 => self.0.render(&body).into(),
            _ => {
                let other = self.original(&inner);
                let other = self.under_root(root, other);
                self.0.render(&other).into()
            }
        };
        // The rules' paths start at the root, where the body may not have it.
        let at_root = match root.trim() {
            "" => body.clone(),
            root => body.get(root).cloned().unwrap_or(Value::Null),
        };
        let mut written = Vec::new();
        let config = if self.chance(4) {
            "disable-image-generation: false\n".to_owned()
        } else {
            self.config(&call, &at_root, &mut written)
        };
        let mut tracked = Vec::new();
        for _ in 0..self.below(4) {
            let path = match self.below(5) {
                0 | 1 if !written.is_empty() => written[self.below(written.len())].clone(),
                2 if !written.is_empty() => {
                    let path: &String = &written[self.below(written.len())];
                    path.split('.').next().unwrap_or_default().to_owned()
                }
                3 => String::new(),
                _ => self.pick(KEYS).to_owned(),
            };
            let path = match root.trim() {
                "" => path,
                root if path.is_empty() => root.to_owned(),
                root => format!("{root}.{path}"),
            };
            tracked.push(Value::from(path));
        }
        let headers: Vec<Value> = call
            .headers
            .iter()
            .map(|(name, value)| json!([name, value]))
            .collect();
        let options = json!({
            "config": config,
            "no_config": self.chance(3),
            "executor": self.pick(EXECUTORS),
            "protocol": call.protocol,
            "from": call.from,
            "root": root,
            "original": original,
            "requested_model": call.requested,
            "request_path": self.pick(REQUEST_PATHS),
            "headers": headers,
            "tracked": tracked,
        });
        let request = self.0.render(&body);
        (call.model, request, options)
    }

    /// `inner` at `root`, mostly.
    fn under_root(&mut self, root: &str, inner: Value) -> Value {
        if root.is_empty() || self.chance(10) {
            inner
        } else {
            let mut map = Map::new();
            map.insert("model".into(), "m".into());
            map.insert(root.trim().into(), inner);
            Value::Object(map)
        }
    }

    /// The client's request: the body with a key gone or added, or another.
    fn original(&mut self, inner: &Value) -> Value {
        let mut map = match inner {
            Value::Object(map) if self.chance(75) => map.clone(),
            _ => self.body(0),
        };
        if !map.is_empty() && self.chance(60) {
            let index = self.below(map.len());
            if let Some(key) = map.keys().nth(index).cloned() {
                map.shift_remove(&key);
            }
        }
        if self.chance(50) {
            let key = self.pick(KEYS);
            let value = self.value(1);
            map.insert(key.into(), value);
        }
        Value::Object(map)
    }

    fn body(&mut self, depth: usize) -> Map<String, Value> {
        let mut map = Map::new();
        for _ in 0..1 + self.below(5) {
            let key = self.pick(KEYS);
            let value = self.value(depth);
            map.insert(key.into(), value);
        }
        map
    }

    fn value(&mut self, depth: usize) -> Value {
        match self.below(if depth < 2 { 12 } else { 6 }) {
            0 => self.pick(STRINGS).into(),
            1 => num(self.pick(NUMBERS)),
            2 => Value::Bool(self.chance(50)),
            3 => Value::Null,
            4 => json!([]),
            5 => json!({}),
            6 | 7 => Value::Object(self.body(depth + 1)),
            8 | 9 => {
                let items = (0..self.below(5)).map(|_| self.item(depth)).collect();
                Value::Array(items)
            }
            _ => {
                let items = (0..1 + self.below(4))
                    .map(|_| {
                        if self.chance(50) {
                            self.pick(QUERY_STRINGS).into()
                        } else {
                            num(self.pick(NUMBERS))
                        }
                    })
                    .collect();
                Value::Array(items)
            }
        }
    }

    /// An array's item: mostly an object with fields to query.
    fn item(&mut self, depth: usize) -> Value {
        if self.chance(15) {
            return self.value(depth + 1);
        }
        let mut map = Map::new();
        for _ in 0..1 + self.below(3) {
            let key = self.pick(ITEM_KEYS);
            let value = match key {
                "v" => num(self.pick(NUMBERS)),
                "type" => self.pick(&["function", "web_search", "custom"]).into(),
                _ => self.pick(QUERY_STRINGS).into(),
            };
            map.insert(key.into(), value);
        }
        if depth < 2 && self.chance(25) {
            let key = self.pick(&["list", "meta", "items"]);
            let value = self.value(depth + 1);
            map.insert(key.into(), value);
        }
        Value::Object(map)
    }

    fn image_tools(&mut self, map: &mut Map<String, Value>) {
        let tools = (0..1 + self.below(3))
            .map(|_| match self.below(6) {
                0 | 1 => json!({ "type": "image_generation", "output_format": "png" }),
                2 => json!({ "type": "function", "name": "image_generation" }),
                3 => json!({ "type": " image_generation " }),
                4 => json!("image_generation"),
                _ => json!({ "type": "web_search" }),
            })
            .collect();
        map.insert("tools".into(), Value::Array(tools));
        if self.chance(70) {
            let choice = match self.below(8) {
                0 => json!("image_generation"),
                1 => json!(" Image_Generation "),
                2 => json!({ "type": "image_generation" }),
                3 => json!({ "type": "tool", "name": "image_generation" }),
                4 => json!({ "type": " TOOL ", "name": " IMAGE_GENERATION " }),
                5 => json!({ "type": "function", "name": "image_generation" }),
                6 => json!("auto"),
                _ => json!(5),
            };
            map.insert("tool_choice".into(), choice);
        }
    }

    /// Tools for the integer pass, in `tools` or an `additional_tools` item.
    fn codex_tools(&mut self, map: &mut Map<String, Value>) {
        let tools: Vec<Value> = (0..1 + self.below(3)).map(|_| self.codex_tool()).collect();
        if self.chance(25) {
            let item = json!({ "type": "additional_tools", "tools": tools });
            map.insert("input".into(), json!([item]));
        } else {
            map.insert("tools".into(), Value::Array(tools));
        }
    }

    /// A Codex tool or another, in one of the formats the integer pass
    /// reads, sometimes in a namespace, named or not, or in two.
    fn codex_tool(&mut self) -> Value {
        let (name, fields) = self.0.rng.pick(CODEX_TOOLS);
        let mut properties = Value::Object(Map::new());
        for field in fields {
            if self.chance(75) {
                let kind = self.pick(CODEX_TYPES);
                self.codex_field(&mut properties, field, kind);
            }
        }
        for _ in 0..self.below(3) {
            let field = self.pick(CODEX_OTHER_FIELDS);
            let kind = self.pick(CODEX_TYPES);
            self.codex_field(&mut properties, field, kind);
        }
        let schema = json!({ "type": "object", "properties": properties });
        let (namespace, name) = match name.split_once("__") {
            Some((namespace, tool)) if self.chance(50) => (Some(namespace), tool),
            _ => (None, name),
        };
        let tool = match self.below(5) {
            0 | 1 => json!({ "type": "function", "name": name, "parameters": schema }),
            2 => json!({ "type": "function", "function": { "name": name, "parameters": schema } }),
            3 => json!({ "name": name, "input_schema": schema }),
            _ if self.chance(50) => {
                json!({ "function_declarations": [{ "name": name, "parameters": schema }] })
            }
            _ => {
                json!({ "functionDeclarations": [{ "name": name, "parametersJsonSchema": schema }] })
            }
        };
        let Some(namespace) = namespace else {
            return tool;
        };
        match self.below(10) {
            0 => json!({ "type": "namespace", "tools": [tool] }),
            1 => json!({ "type": "namespace", "name": "", "tools": [tool] }),
            2 => json!({ "type": "namespace", "name": "outer", "tools": [
                { "type": "namespace", "name": namespace, "tools": [tool] },
            ] }),
            _ => json!({ "type": "namespace", "name": namespace, "tools": [tool] }),
        }
    }

    /// Declares the parameter at `path` in `properties` with the type
    /// `kind`, adding the objects and arrays on the way: an array where the
    /// next step is an index, padded with another branch before it.
    fn codex_field(&mut self, properties: &mut Value, path: &str, kind: &str) {
        let steps: Vec<&str> = path.split('.').collect();
        let mut at = properties;
        for (i, step) in steps.iter().enumerate() {
            let next = match steps.get(i + 1) {
                Some(next) if next.bytes().all(|b| b.is_ascii_digit()) => json!([]),
                _ => json!({}),
            };
            let slot = match at {
                Value::Object(map) => map.entry(step.to_string()).or_insert(next),
                Value::Array(items) => {
                    let index: usize = step.parse().unwrap_or(0);
                    while items.len() <= index {
                        items.push(json!({ "type": "null" }));
                    }
                    let slot = &mut items[index];
                    if !slot.is_object() && !slot.is_array() {
                        *slot = next;
                    }
                    slot
                }
                _ => return,
            };
            at = slot;
        }
        if let Value::Object(map) = at {
            map.insert(
                "type".into(),
                serde_json::from_str(kind).unwrap_or(Value::Null),
            );
            if self.chance(30) {
                map.insert("description".into(), json!("Whole number"));
            }
        }
    }

    /// A config with payload rules, sometimes `disable-image-generation`
    /// too. The paths its defaults and overrides write go in `written`.
    fn config(&mut self, call: &Call, inner: &Value, written: &mut Vec<String>) -> String {
        let mut out = String::new();
        if self.chance(30) {
            out.push_str(&format!(
                "disable-image-generation: {}\n",
                self.pick(IMAGE_MODES)
            ));
        }
        let mut sections = String::new();
        for section in [
            "default",
            "default-raw",
            "override",
            "override-raw",
            "filter",
        ] {
            if !self.chance(45) {
                continue;
            }
            sections.push_str(&format!("  {section}:\n"));
            for _ in 0..1 + self.below(2) {
                sections.push_str("    - models:\n");
                for _ in 0..1 + self.below(2) {
                    let entry = self.model_entry(call, inner);
                    sections.push_str(&entry);
                }
                sections.push_str("      params:\n");
                let params = self.params(section, inner, written);
                sections.push_str(&params);
            }
        }
        if !sections.is_empty() {
            out.push_str("payload:\n");
            out.push_str(&sections);
        }
        if out.is_empty() {
            out.push_str("disable-image-generation: false\n");
        }
        out
    }

    /// A rule's params. Upstream writes a default's or override's params in
    /// Go's random map order, so no two start at the same key, and at most
    /// one starts at a key the body lacks: then no two add keys to the same
    /// object, whose order would depend on theirs.
    fn params(&mut self, section: &str, inner: &Value, written: &mut Vec<String>) -> String {
        let mut out = String::new();
        let mut firsts = Vec::new();
        let mut adds_key = false;
        for _ in 0..1 + self.below(3) {
            if section == "filter" {
                let path = self.path(inner, Use::Filter);
                out.push_str(&format!("        - {}\n", single(&path.text)));
                continue;
            }
            let path = self.path(inner, Use::Write);
            if firsts.contains(&path.first) || (adds_key && !path.first_held) {
                continue;
            }
            adds_key |= !path.first_held;
            firsts.push(path.first);
            let value = if section.ends_with("-raw") {
                self.raw()
            } else {
                self.yaml(0)
            };
            out.push_str(&format!("        {}: {value}\n", single(&path.text)));
            written.push(path.text);
        }
        out
    }

    /// One of a rule's `models` entries.
    fn model_entry(&mut self, call: &Call, inner: &Value) -> String {
        let name = self.model_pattern(call);
        let mut out = format!("        - name: {}\n", single(&name));
        if self.chance(35) {
            let protocol = if self.chance(60) {
                call.protocol
            } else {
                self.pick(RULE_PROTOCOLS)
            };
            out.push_str(&format!("          protocol: {}\n", single(protocol)));
        }
        if self.chance(20) {
            let from = if self.chance(50) {
                call.from
            } else {
                self.pick(RULE_FROM)
            };
            out.push_str(&format!("          from-protocol: {}\n", single(from)));
        }
        if self.chance(20) {
            out.push_str("          headers:\n");
            let mut names = Vec::new();
            for _ in 0..1 + self.below(2) {
                let name = match call.headers.len() {
                    0 => self.pick(HEADER_NAMES),
                    len if self.chance(70) => call.headers[self.below(len)].0,
                    _ => self.pick(HEADER_NAMES),
                };
                if names.contains(&name) {
                    continue;
                }
                names.push(name);
                let pattern = self.pick(HEADER_PATTERNS);
                out.push_str(&format!(
                    "            {}: {}\n",
                    single(name),
                    single(pattern)
                ));
            }
        }
        for (condition, percent) in [("match", 20), ("not-match", 10)] {
            if !self.chance(percent) {
                continue;
            }
            out.push_str(&format!("          {condition}:\n"));
            for _ in 0..1 + self.below(2) {
                let path = self.path(inner, Use::Read);
                let value = match path.at {
                    Some(value) if self.chance(60) => value.to_string(),
                    _ => self.yaml(1),
                };
                out.push_str(&format!("            - {}: {value}\n", single(&path.text)));
            }
        }
        for (condition, percent) in [("exist", 15), ("not-exist", 15)] {
            if !self.chance(percent) {
                continue;
            }
            out.push_str(&format!("          {condition}:\n"));
            for _ in 0..1 + self.below(2) {
                let path = self.path(inner, Use::Read);
                out.push_str(&format!("            - {}\n", single(&path.text)));
            }
        }
        out
    }

    /// A model name pattern, mostly one made from the call's models.
    fn model_pattern(&mut self, call: &Call) -> String {
        if self.chance(25) {
            return self
                .pick(&[
                    "gpt-*",
                    "claude-*",
                    "*-codex",
                    "alias",
                    "alias(high)",
                    "*",
                    "other",
                    "",
                ])
                .to_owned();
        }
        let name = if self.chance(70) {
            call.model
        } else {
            call.requested
        };
        let name = name.trim();
        let chars: Vec<char> = name.chars().collect();
        let cut = self.below(chars.len() + 1);
        let head: String = chars.iter().take(cut).collect();
        let tail: String = chars.iter().skip(cut).collect();
        match self.below(6) {
            0 | 1 => name.to_owned(),
            2 => name.to_uppercase(),
            3 => format!("{head}*"),
            4 => format!("*{tail}"),
            _ => format!("{head}*{tail}"),
        }
    }

    /// A path from `doc`, mostly through what it holds.
    fn path<'d>(&mut self, doc: &'d Value, kind: Use) -> Path<'d> {
        let depth = 1 + self.below(4);
        let mut parts: Vec<String> = Vec::new();
        let mut at = Some(doc);
        // No `#` or wildcard key yet. One may follow a `#(query)` key, which
        // the rules resolve to indexes first, but nothing complex may follow
        // one.
        let mut simple = true;
        for level in 0..depth {
            let last = level + 1 == depth;
            let part = match at {
                Some(Value::Array(items)) => {
                    let (part, next) = self.array_step(items, &mut simple, kind, last);
                    at = next;
                    part
                }
                Some(Value::Object(map)) if !map.is_empty() && self.chance(75) => {
                    let index = self.below(map.len());
                    let (key, next) = map
                        .iter()
                        .nth(index)
                        .map_or(("a", None), |(key, value)| (key.as_str(), Some(value)));
                    at = next;
                    match wildcard(key) {
                        Some(pattern) if level > 0 && simple && self.chance(10) => {
                            simple = false;
                            pattern
                        }
                        _ => escape(key),
                    }
                }
                _ => {
                    let key = if level > 0 && self.chance(15) {
                        self.pick(&["0", "1", "-1"])
                    } else {
                        self.pick(KEYS)
                    };
                    at = match at {
                        Some(Value::Object(map)) => map.get(key),
                        _ => None,
                    };
                    escape(key)
                }
            };
            parts.push(part);
        }
        let first = parts.first().cloned().unwrap_or_default();
        let key = first.replace("\\.", ".");
        Path {
            first_held: doc.get(&key).is_some() && key != "tools" && key != "tool_choice",
            first,
            text: parts.join("."),
            at,
        }
    }

    fn array_step<'d>(
        &mut self,
        items: &'d [Value],
        simple: &mut bool,
        kind: Use,
        last: bool,
    ) -> (String, Option<&'d Value>) {
        let len = items.len();
        match self.below(10) {
            0..=3 if len > 0 => {
                let index = self.below(len);
                (index.to_string(), items.get(index))
            }
            4 => ((len + self.below(2)).to_string(), None),
            5 => ("-1".into(), None),
            6 | 7 if *simple && !(kind == Use::Write && last) => {
                *simple = false;
                ("#".into(), items.first())
            }
            8 | 9 if *simple => match self.query(items) {
                Some(step) => step,
                None => ("0".into(), items.first()),
            },
            _ => ("0".into(), items.first()),
        }
    }

    /// A `#(query)` or `#(query)#` key for one of `items`.
    fn query<'d>(&mut self, items: &'d [Value]) -> Option<(String, Option<&'d Value>)> {
        let item = items.get(self.below(items.len().max(1)))?;
        let all = if self.chance(60) { "#" } else { "" };
        let condition = match item {
            Value::Object(map) => {
                let keys: Vec<&String> = map
                    .keys()
                    .filter(|key| ITEM_KEYS.contains(&key.as_str()))
                    .collect();
                if keys.is_empty() {
                    return None;
                }
                let key = keys[self.below(keys.len())];
                format!("{key}{}", self.comparison(map.get(key)?)?)
            }
            item => self.comparison(item)?,
        };
        Some((format!("#({condition}){all}"), Some(item)))
    }

    /// An operator and a value to compare `value` with.
    fn comparison(&mut self, value: &Value) -> Option<String> {
        match value {
            Value::String(_) => Some(match self.below(4) {
                0 => format!("==\"{}\"", self.pick(QUERY_STRINGS)),
                1 => format!("!=\"{}\"", self.pick(QUERY_STRINGS)),
                2 => format!("%\"{}\"", self.pick(QUERY_PATTERNS)),
                _ => format!("!%\"{}\"", self.pick(QUERY_PATTERNS)),
            }),
            Value::Number(_) => {
                let operator = self.pick(&["==", "!=", "<", ">", "<=", ">="]);
                Some(format!("{operator}{}", self.pick(QUERY_NUMBERS)))
            }
            _ => None,
        }
    }

    /// A YAML value for a default or override.
    fn yaml(&mut self, depth: usize) -> String {
        match self.below(if depth < 2 { 9 } else { 6 }) {
            0 => self.pick(YAML_INTS).to_owned(),
            1 => self.pick(YAML_FLOATS).to_owned(),
            2 => Value::from(self.pick(STRINGS)).to_string(),
            3 if depth == 0 => self.pick(YAML_WORDS).to_owned(),
            3 => format!("'{}'", self.pick(QUERY_STRINGS)),
            4 => self.pick(&["true", "false"]).to_owned(),
            5 => self.pick(&["null", "~"]).to_owned(),
            6 | 7 => {
                let items: Vec<String> = (0..self.below(4)).map(|_| self.yaml(depth + 1)).collect();
                format!("[{}]", items.join(", "))
            }
            _ => {
                let mut keys = YAML_KEYS.to_vec();
                self.0.rng.shuffle(&mut keys);
                let fields: Vec<String> = keys
                    .into_iter()
                    .take(self.below(4))
                    .map(|key| format!("{key}: {}", self.yaml(depth + 1)))
                    .collect();
                format!("{{{}}}", fields.join(", "))
            }
        }
    }

    /// A raw rule's value: mostly a string of JSON, sometimes with spaces
    /// around it, sometimes another YAML value, which upstream writes as
    /// Go encodes it.
    fn raw(&mut self) -> String {
        if self.chance(15) {
            return self.yaml(1);
        }
        let mut text = self.value(1).to_string();
        if self.chance(15) {
            text = format!(" {text}  ");
        }
        single(&text)
    }
}

/// `text` as a YAML single-quoted scalar.
fn single(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `key` as a gjson path key.
fn escape(key: &str) -> String {
    key.replace('.', "\\.")
}

/// A wildcard pattern matching `key`, for keys that make one simply.
fn wildcard(key: &str) -> Option<String> {
    if key.len() < 2 || !key.is_ascii() || key.contains('.') {
        return None;
    }
    let head = key.get(..1)?;
    let rest = key.get(1..key.len() - 1)?;
    Some(if key.len().is_multiple_of(2) {
        format!("{head}*")
    } else {
        format!("{head}{rest}?")
    })
}

#[cfg(test)]
mod tests {
    use open_ferry_core::config::Config;

    use super::*;

    #[test]
    fn cases_are_reproducible_and_their_configs_parse() {
        let first = apply_cases(5, 200);
        let again = apply_cases(5, 200);
        assert_eq!(first.len(), 200);
        for (a, b) in first.iter().zip(&again) {
            assert_eq!(a.request, b.request);
            assert_eq!(a.options, b.options);
            assert!(
                serde_json::from_str::<Value>(&a.request).is_ok(),
                "{}",
                a.name
            );
            let config = a.options["config"].as_str().unwrap_or_default();
            assert!(Config::parse(config).is_ok(), "{}: {config}", a.name);
        }
    }

    #[test]
    fn wildcards_match_their_keys() {
        assert_eq!(wildcard("items").as_deref(), Some("item?"));
        assert_eq!(wildcard("list").as_deref(), Some("l*"));
        assert_eq!(wildcard("a"), None);
        assert_eq!(wildcard("b.c"), None);
        assert_eq!(escape("b.c"), "b\\.c");
    }
}
