// Ported from CLIProxyAPI internal/runtime/executor/helps/payload_helpers.go
// (ApplyPayloadConfigWithTrackedPathsForExecutor, isCodexTargetExecutor,
// PayloadRequestedModel, PayloadRequestPath) and helps/payload_finalizer.go
// (NewPayloadFinalizer) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The config's payload rules applied to the bodies sent upstream:
//! defaults, overrides and filters, by model, protocol, request headers and
//! conditions on the body (upstream's internal/runtime/executor/helps/
//! payload_helpers.go).
//!
//! Each executor calls [`apply`] once per request it sends, as the last
//! change to the body (upstream's final barrier, `NewPayloadFinalizer` or
//! `ApplyPayloadConfigWithTrackedPathsForExecutor` and its wrappers): after
//! the translation and every built-in change, so a rule's conditions see
//! the body as it is sent and nothing undoes what a rule wrote or removed.
//! Only framing (a WebSocket message's `type`), moving a Claude body's
//! `betas` to its header, and serialization follow. A token count's body
//! has the rules applied too. In order, [`apply`]:
//! 1. declares a Codex client's whole-number tool parameters `integer`
//!    again (the translation can move them to where the pass before it
//!    didn't look), unless the body goes to a Codex executor;
//! 2. with `disable-image-generation`, takes the built-in
//!    `image_generation` tool out of `tools` and `tool_choice`, so that a
//!    rule can put it back;
//! 3. applies the `default` and `default-raw` rules, each writing a path
//!    the client's request doesn't have, the first rule to write a path
//!    winning;
//! 4. applies the `override` and `override-raw` rules, the last to write a
//!    path winning;
//! 5. applies the `filter` rules, removing paths.
//!
//! An image or video body, JSON or a `multipart/form-data` form, has the
//! rules applied with [`apply_media`] instead, which shows the rules a form
//! as a JSON object and writes the form again only if they change it (see
//! the `media` module).
//!
//! A rule applies when one of its models matches the model sent upstream,
//! or the model the client named with or without its thinking suffix, and
//! the entry's protocol, client protocol, headers and conditions on the
//! body hold. Paths are gjson and sjson paths (see the `gjson` and `sjson`
//! modules), under the executor's root, and a `#(query)` key stands for the index of
//! each array item the query matches.
//!
//! A rule writes exactly the value the operator configured, the same on
//! every request. That includes identity-shaped fields such as
//! `metadata.user_id`, `user`, `safety_identifier` or `prompt_cache_key`,
//! when the operator writes them; open-ferry never generates or derives a
//! value for them. A rule's `headers` are only read, to decide whether it
//! applies; no rule writes a header.
//!
//! A rule can write any field of a body, `betas` of a Claude body among
//! them. The Claude executor reads the body after the rules have run, so a
//! `betas` a rule wrote is taken out of it and sent in the `anthropic-beta`
//! header, merged with the client's, exactly as a `betas` the client sent is.
//! Upstream does the same, and nothing filters either: the operator who
//! configures the rule chooses the betas, as they choose any other value.
//!
//! The `Debug` of a [`Call`] and of [`Rules`] shows header names and the
//! kind and size of a value, never what a header, param or condition holds,
//! as any of them can be a credential.
//!
//! The rules are compiled once per config load: the binary calls
//! [`reconfigure`] with each config it loads, and every call reads the
//! rules installed then, so a reload takes effect on the next request.
//!
//! Deviations from upstream:
//! - The rules come from the config last given to [`reconfigure`], where
//!   upstream reads the executor's own config. A reload that changes only
//!   the payload rules registers the Codex executors again but not the
//!   others, which keep the config they were made with. An executor's own
//!   config is read only when no config has been installed, as in tests.
//! - A rule's params apply in the order the file gives them; Go iterates
//!   its map in random order.
//! - A value that can't be written as JSON is dropped when the config
//!   loads, with a warning (see [`Rules`]).
//! - A param to write whose path has more than 64 keys is dropped the same
//!   way, where upstream builds a value of any depth. One 2,000 levels deep
//!   overflows the stack of a thread that builds, writes or drops it.
//! - Some gjson and sjson syntax isn't read, and some paths upstream
//!   writes wrongly change nothing (see the `gjson` and `sjson` modules).
//! - The client's request is translated for the default rules' check only
//!   when one of them comes to a path; upstream translates it for every
//!   call.
//! - Every executor names itself in [`Target::executor`]; upstream names
//!   only some (Codex's, xAI's, and each executor's token count), and only
//!   the Codex names change anything.

mod gjson;
mod image;
mod matchers;
mod media;
mod path;
mod query;
mod rules;
mod sjson;
#[cfg(test)]
mod tests;

pub use media::{MediaError, MediaTarget, apply_media};
pub use rules::Rules;

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use http::HeaderMap;
use http::header::{HeaderValue, USER_AGENT};
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Format, Options, Request};
use open_ferry_translate::codex_client::{header_value, tool_integers};
use open_ferry_translate::go::to_lower;
use open_ferry_translate::registry::Registry;
use serde_json::Value;

use crate::codex::compat;
use crate::codex::request::parse_object;
use matchers::Context;

/// Where a body goes: what the rules match against besides the request.
#[derive(Clone, Copy)]
pub struct Target<'a> {
    /// The executor's identifier, such as `claude`, `codex` or
    /// `codex-websockets` (upstream's `targetExecutor`).
    pub executor: &'a str,
    /// The format the body is in (upstream's `protocol`).
    pub protocol: &'a Format,
    /// The model sent upstream, without a thinking suffix.
    pub model: &'a str,
    /// The path the rules' paths are under; empty for the body's root.
    pub root: &'a str,
    /// Whether the body asks for a stream, as the client's original
    /// request is translated for the defaults' checks.
    pub stream: bool,
    /// The paths the caller wants to know an applied rule wrote or deleted
    /// (upstream's `trackedPaths`).
    pub tracked: &'a [&'a str],
    /// How the executor translates a client body, for the defaults'
    /// checks; `None` for the usual translation, which readies a Codex
    /// client's request and translates it to [`Target::protocol`].
    pub translate: Option<&'a dyn Fn(Value) -> Value>,
}

impl fmt::Debug for Target<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Target")
            .field("executor", &self.executor)
            .field("protocol", &self.protocol)
            .field("model", &self.model)
            .field("root", &self.root)
            .field("stream", &self.stream)
            .field("tracked", &self.tracked)
            .field("translate", &self.translate.map(|_| ".."))
            .finish()
    }
}

/// The tracked paths an applied rule wrote or deleted, or wrote or deleted
/// something under.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Touched(BTreeSet<String>);

impl Touched {
    /// Whether a rule touched the tracked `path`.
    pub fn contains(&self, path: &str) -> bool {
        self.0.contains(path)
    }

    /// Whether no rule touched a tracked path.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The tracked paths touched, in order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

/// A call the rules are applied for, as upstream's
/// `ApplyPayloadConfigWithTrackedPathsForExecutor` takes it.
///
/// Its `Debug` shows the names of the headers and the size of their values,
/// not the values, which can hold a credential.
#[derive(Clone, Copy)]
pub struct Call<'a> {
    /// The executor's identifier (`targetExecutor`).
    pub executor: &'a str,
    /// The format the body is in (`protocol`).
    pub protocol: &'a str,
    /// The client's format (`fromProtocol`).
    pub from: &'a str,
    /// The model sent upstream (`model`).
    pub model: &'a str,
    /// The model the client named (`requestedModel`).
    pub requested_model: &'a str,
    /// The client's route (`requestPath`).
    pub request_path: &'a str,
    /// The path the rules' paths are under (`root`).
    pub root: &'a str,
    /// The client's request headers (`headers`).
    pub headers: &'a HeaderMap,
    /// The paths to report on (`trackedPaths`).
    pub tracked: &'a [&'a str],
}

impl fmt::Debug for Call<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Call")
            .field("executor", &self.executor)
            .field("protocol", &self.protocol)
            .field("from", &self.from)
            .field("model", &self.model)
            .field("requested_model", &self.requested_model)
            .field("request_path", &self.request_path)
            .field("root", &self.root)
            .field("headers", &HeaderSizes(self.headers))
            .field("tracked", &self.tracked)
            .finish()
    }
}

/// The size of a byte string, which is all its `Debug` says of it.
struct Size(usize);

impl fmt::Debug for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} bytes>", self.0)
    }
}

/// Header names with the size of each value, for a `Debug` that must not
/// show a credential.
struct HeaderSizes<'a>(&'a HeaderMap);

impl fmt::Debug for HeaderSizes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(
                self.0
                    .iter()
                    .map(|(name, value)| (name.as_str(), Size(value.len()))),
            )
            .finish()
    }
}

/// What a JSON value is, not what it holds, for a `Debug` that must not
/// show a value a rule writes or compares.
struct Shape<'a>(&'a Value);

impl fmt::Debug for Shape<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Value::Null => f.write_str("null"),
            Value::Bool(_) => f.write_str("bool"),
            Value::Number(_) => f.write_str("number"),
            Value::String(text) => write!(f, "string({} bytes)", text.len()),
            Value::Array(items) => write!(f, "array({} items)", items.len()),
            Value::Object(map) => write!(f, "object({} keys)", map.len()),
        }
    }
}

/// The rules installed by the last [`reconfigure`].
static CURRENT: RwLock<Option<Arc<Rules>>> = RwLock::new(None);

/// Compiles `config`'s payload rules, warning about the values dropped,
/// and installs them for every call from now on.
pub fn reconfigure(config: &Config) {
    let rules = Arc::new(Rules::compile(config));
    *CURRENT.write().unwrap_or_else(PoisonError::into_inner) = Some(rules);
}

/// The rules [`reconfigure`] installed, if it has run.
fn installed() -> Option<Arc<Rules>> {
    CURRENT
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Applies the payload rules to `body`, the translated `request` made with
/// `options`, going to `target`, and says which tracked paths they touched
/// (upstream's `ApplyPayloadConfigWithTrackedPathsForExecutor`). With no
/// `config`, only a Codex client's tool parameters are changed.
pub fn apply(
    config: Option<&Config>,
    target: &Target<'_>,
    request: &Request,
    options: &Options,
    body: &mut Value,
) -> Touched {
    let rules = select(config, installed);
    let requested = requested_model(request, options);
    let call = Call {
        executor: target.executor,
        protocol: target.protocol.as_str(),
        from: options.source_format.as_str(),
        model: target.model,
        requested_model: &requested,
        request_path: options.metadata.request_path.trim(),
        root: target.root,
        headers: &options.headers,
        tracked: target.tracked,
    };
    apply_call(
        rules.get(),
        &call,
        || original(config, target, request, options),
        body,
    )
}

/// The rules a call reads.
enum Selected {
    /// No config: no rules.
    None,
    /// The rules [`reconfigure`] installed.
    Installed(Arc<Rules>),
    /// The executor's config's rules, compiled for the call.
    Own(Rules),
}

impl Selected {
    fn get(&self) -> Option<&Rules> {
        match self {
            Selected::None => None,
            Selected::Installed(rules) => Some(rules),
            Selected::Own(rules) => Some(rules),
        }
    }
}

/// The rules for a call with the executor's `config`: none without one,
/// else those `installed` gives, else `config`'s own, compiled for the
/// call without the warnings [`reconfigure`] gives once per load.
fn select(config: Option<&Config>, installed: impl FnOnce() -> Option<Arc<Rules>>) -> Selected {
    let Some(config) = config else {
        return Selected::None;
    };
    match installed() {
        Some(rules) => Selected::Installed(rules),
        None => Selected::Own(Rules::build(config, false)),
    }
}

/// The model the client named, else the request's, trimmed
/// (`PayloadRequestedModel`).
fn requested_model(request: &Request, options: &Options) -> String {
    let named = options.metadata.requested_model.trim();
    if named.is_empty() {
        request.model.trim().to_owned()
    } else {
        named.to_owned()
    }
}

/// The client's request, as it arrived, translated as the executor
/// translates its body; `None` when there was none.
fn original(
    config: Option<&Config>,
    target: &Target<'_>,
    request: &Request,
    options: &Options,
) -> Option<Value> {
    let raw = if options.original_request.is_empty() {
        &request.payload
    } else {
        &options.original_request
    };
    if raw.is_empty() {
        return None;
    }
    let mut payload = parse_object(raw);
    Some(match target.translate {
        Some(translate) => translate(payload),
        None => {
            compat::before_translation(config, options, target.protocol, &mut payload);
            Registry::global().translate_request(
                &options.source_format,
                target.protocol,
                target.model,
                payload,
                target.stream,
            )
        }
    })
}

/// Whether `executor` is a Codex executor, whose clients' tools are left
/// as they are (`isCodexTargetExecutor`).
fn is_codex_target(executor: &str) -> bool {
    matches!(
        to_lower(executor.trim()).as_str(),
        "codex" | "codex-websockets" | "codex_websockets"
    )
}

/// What the default rules check a path against: the client's request,
/// translated, or else the body as it was before the rules, made only
/// when first needed.
struct Source<F> {
    original: Option<F>,
    before_strip: Option<Value>,
    value: Option<Value>,
}

impl<F: FnOnce() -> Option<Value>> Source<F> {
    /// Whether the client's request has `path`.
    fn has(&mut self, body: &Value, path: &str) -> bool {
        let source = self.value.get_or_insert_with(|| {
            self.original
                .take()
                .and_then(|original| original())
                .or_else(|| self.before_strip.take())
                .unwrap_or_else(|| body.clone())
        });
        gjson::get(source, path).is_some()
    }
}

/// [`apply`] for a call described in full: `rules` (`None` for no
/// config), the call, the client's request translated, made only if a
/// default rule needs it, and the body.
pub fn apply_call(
    rules: Option<&Rules>,
    call: &Call<'_>,
    original: impl FnOnce() -> Option<Value>,
    body: &mut Value,
) -> Touched {
    let mut touched = Touched::default();
    if !is_codex_target(call.executor) {
        let user_agent = header_value(
            call.headers
                .get_all(USER_AGENT)
                .iter()
                .map(HeaderValue::as_bytes),
        );
        if tool_integers::normalize(body, &user_agent) {
            tracing::debug!("payload: normalized Codex client tool number types to integer");
        }
    }
    let Some(rules) = rules else {
        return touched;
    };

    let mut before_strip = None;
    if image::should_strip(rules.image, call.request_path)
        && let Some(strip) = image::plan(body, call.root)
    {
        if rules.has_defaults() {
            before_strip = Some(body.clone());
        }
        strip.apply(body);
    }

    if !rules.has_rules() {
        return touched;
    }
    let candidates = matchers::candidates(call.model, call.requested_model);
    if candidates.is_empty() {
        return touched;
    }
    let context = Context {
        protocol: call.protocol,
        from: call.from,
        headers: call.headers,
        root: call.root,
        candidates: &candidates,
    };
    let mut mark = |resolved: &str| {
        for tracked in call.tracked {
            let tracked = tracked.trim();
            if !tracked.is_empty() && path::targets_path(resolved, tracked) {
                touched.0.insert(tracked.to_owned());
            }
        }
    };
    let mut source = Source {
        original: Some(original),
        before_strip,
        value: None,
    };

    // Defaults: the first write of a path wins, across both kinds.
    let mut applied = HashSet::new();
    for rule in rules.default.iter().chain(&rules.default_raw) {
        if !matchers::rules_match(&rule.models, &context, body) {
            continue;
        }
        for (path, value) in &rule.params {
            let full = path::build_path(call.root, path);
            if full.is_empty() {
                continue;
            }
            for resolved in path::resolve(body, &full) {
                if source.has(body, &resolved) || applied.contains(&resolved) {
                    continue;
                }
                if sjson::set(body, &resolved, value).is_err() {
                    continue;
                }
                mark(&resolved);
                applied.insert(resolved);
            }
        }
    }

    // Overrides: the last write of a path wins.
    for rule in rules.overrides.iter().chain(&rules.override_raw) {
        if !matchers::rules_match(&rule.models, &context, body) {
            continue;
        }
        for (path, value) in &rule.params {
            let full = path::build_path(call.root, path);
            if full.is_empty() {
                continue;
            }
            for resolved in path::resolve(body, &full) {
                if sjson::set(body, &resolved, value).is_ok() {
                    mark(&resolved);
                }
            }
        }
    }

    // Filters, each path's matches removed from the last.
    for rule in &rules.filter {
        if !matchers::rules_match(&rule.models, &context, body) {
            continue;
        }
        for path in &rule.params {
            let full = path::build_path(call.root, path);
            if full.is_empty() {
                continue;
            }
            for resolved in path::resolve(body, &full).iter().rev() {
                if sjson::delete(body, resolved).is_ok() {
                    mark(resolved);
                }
            }
        }
    }
    touched
}
