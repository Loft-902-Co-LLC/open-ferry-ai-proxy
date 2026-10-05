// Ported from CLIProxyAPI internal/runtime/executor/helps/response_model.go
// (extractResponseModelEvent, extractClaudeResponseModelEvent,
// extractGeminiResponseModelEvent, extractGenericResponseModelEvent,
// isInteractionsTerminal, extractCodexResponseModelEvent,
// codexResponseModelEventKind, normalizeModelName, stripModelProviderPrefix,
// IsModelSubstituted, isDatedModelAlias, isModelDateSuffix,
// isModelNumericVersionSuffix, isModelDigits,
// codexModelSubstitutionThrottle) and stream_response_model_observer.go
// (StreamResponseModelObserver) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The model an upstream says it served, read from its answer, and whether
//! it differs from the model asked for.
//!
//! Each provider names the served model in its own place: Codex in the
//! `response` object of its `response.*` events, Claude in `message_start`,
//! Gemini in `modelVersion`, and the OpenAI-compatible protocols in
//! `model`. The first name read is kept, and reading stops at the event
//! that ends the answer. When the served model isn't the one asked for, or
//! a dated snapshot of it, the usage reporter warns, at most once per
//! credential and model pair in ten minutes ([`Throttle`]).
//!
//! Deviations from upstream:
//! - JSON that doesn't parse whole names no model and ends nothing, so a
//!   Claude `message_stop` cut short isn't the end; gjson reads what it can
//!   (see [`super::json`]).
//! - The throttle belongs to the [`super::Usage`], where upstream keeps one
//!   for the process.
//! - [`StreamResponseModelObserver`] keeps the model it reads itself, where
//!   upstream's writes it into a usage reporter.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use open_ferry_translate::go;
use open_ferry_translate::thinking::base_model_name;

use super::json::{self, Doc, Node};
use super::parse::json_payload;

/// The longest served model name kept; a longer one, which no known model
/// has, isn't trusted (upstream's `maxResponseModelLength`).
pub(crate) const MAX_RESPONSE_MODEL_LENGTH: usize = 128;

/// How long one credential and model pair stays quiet after a warning
/// (upstream's `modelSubstitutionWarnWindow`).
pub(crate) const WARN_WINDOW: Duration = Duration::from_secs(10 * 60);

/// How many pairs the throttle remembers (upstream's
/// `modelSubstitutionWarnMaxEntries`).
pub(crate) const WARN_MAX_ENTRIES: usize = 1024;

/// The model `payload` (a JSON frame or an SSE line) says was served, empty
/// when it names none, and whether its event ends the answer, read as
/// `provider`'s upstream writes them (upstream's
/// `extractResponseModelEvent`).
pub(crate) fn extract_response_model_event(payload: &[u8], provider: &str) -> (String, bool) {
    let Some(data) = json_payload(payload) else {
        return (String::new(), false);
    };
    match go::to_lower(provider.trim()).as_str() {
        "codex" => extract_codex_response_model_event(payload),
        "claude" => extract_claude_response_model_event(data),
        "gemini" | "gemini-interactions" | "vertex" | "aistudio" | "antigravity" => {
            extract_gemini_response_model_event(data)
        }
        _ => extract_generic_response_model_event(data),
    }
}

/// A string value trimmed, when it is a string no longer than the bound.
fn served_name(node: Node<'_>) -> Option<String> {
    if !node.is_string() {
        return None;
    }
    let served = node.string().trim().to_owned();
    (served.len() <= MAX_RESPONSE_MODEL_LENGTH).then_some(served)
}

/// Claude's model, from `message_start`, or from a whole `message`
/// (upstream's `extractClaudeResponseModelEvent`).
pub(crate) fn extract_claude_response_model_event(data: &[u8]) -> (String, bool) {
    let doc = Doc::scan(data);
    match doc.get("type").string().as_ref() {
        "message_start" => (
            served_name(doc.get("message.model")).unwrap_or_default(),
            false,
        ),
        "message_stop" => (String::new(), true),
        "message" => (served_name(doc.get("model")).unwrap_or_default(), true),
        _ => {
            if !json::valid(data) {
                return (String::new(), false);
            }
            let message_model = doc.get("message.model");
            let served = if message_model.is_string() {
                served_name(message_model)
            } else {
                served_name(doc.get("model"))
            };
            (served.unwrap_or_default(), false)
        }
    }
}

/// Gemini's model version, and whether a candidate finished (upstream's
/// `extractGeminiResponseModelEvent`).
pub(crate) fn extract_gemini_response_model_event(data: &[u8]) -> (String, bool) {
    if !json::valid(data) {
        return (String::new(), false);
    }
    let doc = Doc::scan(data);
    let model = ["response.modelVersion", "modelVersion", "interaction.model"]
        .into_iter()
        .map(|path| doc.get(path))
        .find(|node| node.is_string())
        .unwrap_or_else(|| doc.get("model"));
    let mut served = String::new();
    if model.is_string() {
        served = model.string().trim().to_owned();
        if served.len() > MAX_RESPONSE_MODEL_LENGTH {
            served.clear();
        }
    }
    let mut finish = doc.get("candidates.0.finishReason");
    if !finish.exists() {
        finish = doc.get("response.candidates.0.finishReason");
    }
    let mut terminal = finish.exists() && !finish.string().is_empty();
    if !terminal {
        terminal =
            is_interactions_terminal(&event_type(&doc), &doc.get("interaction.status").string());
    }
    (served, terminal)
}

/// The `event_type`, else the `type`.
fn event_type(doc: &Doc) -> String {
    let event_type = doc.get("event_type").string();
    if event_type.is_empty() {
        doc.get("type").string().into_owned()
    } else {
        event_type.into_owned()
    }
}

/// The model of an OpenAI-style answer or event, wherever it is (upstream's
/// `extractGenericResponseModelEvent`).
pub(crate) fn extract_generic_response_model_event(data: &[u8]) -> (String, bool) {
    if !json::valid(data) {
        return (String::new(), false);
    }
    let doc = Doc::scan(data);
    if let Some(served) = served_name(doc.get("response.model")) {
        let kind = doc.get("type").string();
        let terminal = matches!(
            kind.as_ref(),
            "response.completed" | "response.done" | "response.incomplete"
        );
        return (served, terminal);
    }
    if let Some(served) = served_name(doc.get("interaction.model")) {
        let terminal =
            is_interactions_terminal(&event_type(&doc), &doc.get("interaction.status").string());
        return (served, terminal);
    }
    if let Some(served) = served_name(doc.get("modelVersion")) {
        let finish = doc.get("candidates.0.finishReason");
        return (served, finish.exists() && !finish.string().is_empty());
    }
    if let Some(served) = served_name(doc.get("response.modelVersion")) {
        let finish = doc.get("response.candidates.0.finishReason");
        return (served, finish.exists() && !finish.string().is_empty());
    }
    if let Some(served) = served_name(doc.get("message.model")) {
        return (served, false);
    }
    if let Some(served) = served_name(doc.get("model")) {
        let status = doc.get("status").string();
        let terminal = doc.get("object").string() == "chat.completion"
            || !doc.get("choices.0.finish_reason").string().is_empty()
            || status == "completed"
            || status == "incomplete";
        return (served, terminal);
    }
    let event_type = event_type(&doc);
    let terminal = is_interactions_terminal(&event_type, &doc.get("interaction.status").string())
        || event_type == "message_stop";
    (String::new(), terminal)
}

/// Whether an Interactions event or status ends the interaction (upstream's
/// `isInteractionsTerminal`).
fn is_interactions_terminal(event_type: &str, status: &str) -> bool {
    matches!(
        event_type,
        "interaction.completed"
            | "interaction.done"
            | "interaction.failed"
            | "interaction.cancelled"
    ) || matches!(status, "completed" | "incomplete" | "cancelled" | "failed")
}

/// The model of a Codex event that carries the whole response, and whether
/// the event ends it (upstream's `extractCodexResponseModelEvent`).
pub(crate) fn extract_codex_response_model_event(payload: &[u8]) -> (String, bool) {
    let Some(data) = json_payload(payload) else {
        return (String::new(), false);
    };
    let doc = Doc::scan(data);
    let (carries_model, terminal) = codex_event_kind(&doc.get("type").string());
    if !carries_model || !json::valid(data) {
        return (String::new(), false);
    }
    (
        served_name(doc.get("response.model")).unwrap_or_default(),
        terminal,
    )
}

/// Whether a Codex event carries the response object, and whether it ends
/// the response (upstream's `codexResponseModelEventKind`).
fn codex_event_kind(event_type: &str) -> (bool, bool) {
    match event_type.trim() {
        "response.created" | "response.in_progress" => (true, false),
        "response.completed" | "response.incomplete" | "response.done" => (true, true),
        _ => (false, false),
    }
}

/// A model name lower-cased, without its thinking suffix (upstream's
/// `normalizeModelName`).
pub(crate) fn normalize_model_name(model: &str) -> String {
    let lower = go::to_lower(model.trim());
    base_model_name(&lower).trim().to_owned()
}

/// The model's name after its last `/`, when something follows it.
fn strip_model_provider_prefix(model: &str) -> &str {
    match model.rfind('/') {
        Some(slash) if slash + 1 < model.len() => model.get(slash + 1..).unwrap_or(model),
        _ => model,
    }
}

/// Whether the upstream served a model other than `requested`: the same
/// model, a dated snapshot of it, or either without a provider prefix or a
/// `-latest` suffix, isn't another (upstream's `IsModelSubstituted`).
pub fn is_model_substituted(requested: &str, served: &str) -> bool {
    let served = normalize_model_name(served);
    if served.is_empty() {
        return false;
    }
    let requested = normalize_model_name(requested);
    if requested.is_empty() || requested == served {
        return false;
    }
    let aliases = |a: &str, b: &str| is_dated_model_alias(a, b) || is_dated_model_alias(b, a);
    if aliases(&requested, &served) {
        return false;
    }
    let requested = strip_model_provider_prefix(&requested);
    let served = strip_model_provider_prefix(&served);
    if requested == served || aliases(requested, served) {
        return false;
    }
    let requested = requested.strip_suffix("-latest").unwrap_or(requested);
    let served = served.strip_suffix("-latest").unwrap_or(served);
    requested != served && !aliases(requested, served)
}

/// Whether `dated` is `base`, a dash, and a date or a three-digit version
/// (upstream's `isDatedModelAlias`).
fn is_dated_model_alias(base: &str, dated: &str) -> bool {
    let Some(suffix) = dated
        .strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return false;
    };
    is_model_date_suffix(suffix) || (suffix.len() == 3 && is_model_digits(suffix))
}

/// Whether `suffix` is a `YYYY-MM-DD` or `YYYYMMDD` date (upstream's
/// `isModelDateSuffix`).
fn is_model_date_suffix(suffix: &str) -> bool {
    let bytes = suffix.as_bytes();
    match bytes.len() {
        10 => {
            bytes.get(4) == Some(&b'-')
                && bytes.get(7) == Some(&b'-')
                && [0..4, 5..7, 8..10]
                    .into_iter()
                    .all(|range| bytes.get(range).is_some_and(is_ascii_digits))
        }
        8 => is_ascii_digits(bytes),
        _ => false,
    }
}

/// Whether `value` is a run of ASCII digits, not empty.
fn is_model_digits(value: &str) -> bool {
    is_ascii_digits(value.as_bytes())
}

fn is_ascii_digits(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit)
}

/// The model an answer says it served, as the usage reporter keeps it: the
/// first name read, until the event that ends the answer.
#[derive(Clone, Debug, Default)]
pub(crate) struct ResponseModel {
    served: String,
    done: bool,
}

impl ResponseModel {
    /// Reads `payload` as `provider`'s upstream writes it (upstream's
    /// `ObserveResponseModel`).
    pub(crate) fn observe(&mut self, payload: &[u8], provider: &str) {
        if self.done {
            return;
        }
        let (served, terminal) = extract_response_model_event(payload, provider);
        if !served.is_empty() {
            self.served = served;
        }
        if terminal {
            self.done = true;
        }
    }

    /// Takes `model` as the served model, unless reading has ended
    /// (upstream's `SetResponseModel`).
    pub(crate) fn set(&mut self, model: &str) {
        if self.done {
            return;
        }
        let model = model.trim();
        if !model.is_empty() && model.len() <= MAX_RESPONSE_MODEL_LENGTH {
            self.served = model.to_owned();
        }
    }

    /// The served model, empty when none was read.
    pub(crate) fn get(&self) -> &str {
        &self.served
    }

    /// Whether reading has ended (upstream's `IsResponseModelFinal`).
    pub(crate) fn is_final(&self) -> bool {
        self.done
    }
}

/// What the throttle tells warnings apart by.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ThrottleKey {
    pub(crate) provider: String,
    pub(crate) auth_id: String,
    pub(crate) requested: String,
    pub(crate) served: String,
}

/// The clock the throttle reads.
pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// When each credential and model pair last warned of a substitution
/// (upstream's `codexModelSubstitutionThrottle`).
pub(crate) struct Throttle {
    clock: Clock,
    last_warn: Mutex<HashMap<ThrottleKey, Instant>>,
}

impl Throttle {
    /// A throttle reading `clock`.
    pub(crate) fn new(clock: Clock) -> Self {
        Self {
            clock,
            last_warn: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `key` may warn now, writing down that it did. Past the
    /// bound, pairs quiet for the window are forgotten, and all of them
    /// when that isn't enough.
    pub(crate) fn allow(&self, key: ThrottleKey) -> bool {
        let mut last_warn = self
            .last_warn
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let now = (self.clock)();
        if let Some(last) = last_warn.get(&key)
            && now.saturating_duration_since(*last) < WARN_WINDOW
        {
            return false;
        }
        if last_warn.len() >= WARN_MAX_ENTRIES {
            last_warn.retain(|_, at| now.saturating_duration_since(*at) < WARN_WINDOW);
            if last_warn.len() >= WARN_MAX_ENTRIES {
                last_warn.clear();
            }
        }
        last_warn.insert(key, now);
        true
    }

    /// How many pairs it remembers.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.last_warn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new(Arc::new(Instant::now))
    }
}

impl fmt::Debug for Throttle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Throttle").finish_non_exhaustive()
    }
}

/// How many bytes of a line, or of an event's data, are kept (upstream's
/// `defaultMaxStreamModelBufferBound`).
pub const STREAM_MODEL_BUFFER_BOUND: usize = 64 * 1024;

/// How many data lines one event may have (upstream's
/// `defaultMaxLinesPerStreamEvent`).
pub const MAX_LINES_PER_STREAM_EVENT: usize = 2048;

/// What each data line costs besides its bytes (upstream's
/// `streamEventLineOverhead`).
const STREAM_EVENT_LINE_OVERHEAD: usize = 32;

/// Reads the served model from a stream's chunks wherever they split it:
/// each whole line, and each event whose data spans lines joined, within a
/// bound (upstream's `StreamResponseModelObserver`). A line longer than the
/// bound is skipped, and an event larger than it is skipped to its end.
///
/// Upstream reads OpenAI-compatible image streams with it, which aren't
/// ported yet.
#[derive(Debug)]
pub struct StreamResponseModelObserver {
    provider: String,
    model: ResponseModel,
    buf: Vec<u8>,
    frame: Vec<Vec<u8>>,
    frame_bytes: usize,
    max_bound: usize,
    overflow: bool,
    event_overflow: bool,
}

impl StreamResponseModelObserver {
    /// An observer reading the stream as `provider`'s upstream writes it.
    pub fn new(provider: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            model: ResponseModel::default(),
            buf: Vec::new(),
            frame: Vec::new(),
            frame_bytes: 0,
            max_bound: STREAM_MODEL_BUFFER_BOUND,
            overflow: false,
            event_overflow: false,
        }
    }

    /// The served model read so far, empty when none was.
    pub fn response_model(&self) -> &str {
        self.model.get()
    }

    /// Whether the event that ends the answer was read.
    pub fn is_final(&self) -> bool {
        self.model.is_final()
    }

    /// The data lines of the event being read, for tests.
    #[cfg(test)]
    pub(crate) fn frame_len(&self) -> usize {
        self.frame.len()
    }

    fn bound(&self) -> usize {
        if self.max_bound == 0 {
            STREAM_MODEL_BUFFER_BOUND
        } else {
            self.max_bound
        }
    }

    fn drop_frame(&mut self) {
        if !self.frame.is_empty() {
            self.event_overflow = true;
            self.frame = Vec::new();
            self.frame_bytes = 0;
        }
    }

    /// Reads the next chunk (upstream's `Feed`).
    pub fn feed(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        if self.model.is_final() {
            self.buf = Vec::new();
            self.frame = Vec::new();
            return;
        }
        let mut chunk = chunk;
        while !chunk.is_empty() {
            let newline = chunk.iter().position(|&b| b == b'\n');
            if self.overflow {
                let Some(newline) = newline else {
                    return;
                };
                chunk = chunk.get(newline + 1..).unwrap_or_default();
                self.overflow = false;
                self.buf.clear();
                continue;
            }
            let Some(newline) = newline else {
                if self.buf.len() + chunk.len() > self.bound() {
                    self.overflow = true;
                    self.buf.clear();
                    self.drop_frame();
                } else {
                    self.buf.extend_from_slice(chunk);
                }
                return;
            };
            let line_part = chunk.get(..newline).unwrap_or_default();
            chunk = chunk.get(newline + 1..).unwrap_or_default();
            if self.buf.len() + line_part.len() > self.bound() {
                self.buf.clear();
                self.drop_frame();
                continue;
            }
            let line = if self.buf.is_empty() {
                line_part.to_vec()
            } else {
                self.buf.extend_from_slice(line_part);
                std::mem::take(&mut self.buf)
            };
            self.handle_line(trim_cr(&line));
            if self.model.is_final() {
                self.buf = Vec::new();
                self.frame = Vec::new();
                return;
            }
        }
    }

    /// Reads what is left at the stream's end (upstream's `Finish`).
    pub fn finish(&mut self) {
        if !self.overflow && !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.handle_line(trim_cr(&line));
        }
        self.flush_event();
    }

    fn handle_line(&mut self, line: &[u8]) {
        let trimmed = json::trim_space(line);
        if trimmed.is_empty() {
            self.flush_event();
            return;
        }
        if self.event_overflow {
            return;
        }
        self.model.observe(line, &self.provider);
        if let Some(data) = trimmed.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            let cost = data.len() + STREAM_EVENT_LINE_OVERHEAD;
            if self.frame_bytes + cost > self.bound()
                || self.frame.len() >= MAX_LINES_PER_STREAM_EVENT
            {
                self.event_overflow = true;
                self.frame = Vec::new();
                self.frame_bytes = 0;
                return;
            }
            self.frame.push(data.to_vec());
            self.frame_bytes += cost;
        }
    }

    fn flush_event(&mut self) {
        if self.event_overflow {
            self.event_overflow = false;
            self.frame = Vec::new();
            self.frame_bytes = 0;
            return;
        }
        if self.frame.is_empty() {
            return;
        }
        if self.frame.len() > 1 {
            let joined = self.frame.join(&b'\n');
            self.model.observe(&joined, &self.provider);
        }
        self.frame = Vec::new();
        self.frame_bytes = 0;
    }
}

/// `line` without one trailing carriage return.
fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}
