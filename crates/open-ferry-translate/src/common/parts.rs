// Ported from CLIProxyAPI internal/translator/common/parts.go (UnsupportedPartError,
// UserTurnDrops, UserRun, IsInteractionsInstructionStep, InteractionsAttachmentType,
// GeminiPartIsSendable, CountSendableGeminiParts, IsHTTPURL) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What a translator does with a content part it can't send: the rest of the
//! user turn still goes out, but a user turn left with nothing to send is
//! refused with [`UnsupportedPartError`] rather than forwarded empty.
//!
//! Deviations from upstream:
//! - [`is_http_url`] checks the parts of Go's `url.Parse` that decide the
//!   answer (no control characters, a scheme, `//` and a host with only the
//!   characters and port Go accepts) instead of parsing the whole URL.

use std::error::Error;
use std::fmt;

use serde_json::Value;

use crate::json::{path, str_of};

/// A request-scoped refusal: the request has a part of this type, and the
/// translation has no equivalent for it, so the request must not be sent with
/// the part removed. Its status is 400.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnsupportedPartError {
    /// The part's type, such as `file` or `container_upload`.
    pub part_type: String,
}

impl UnsupportedPartError {
    /// A refusal for a part of `part_type`.
    pub fn new(part_type: impl Into<String>) -> Self {
        Self {
            part_type: part_type.into(),
        }
    }

    /// The HTTP status for the refusal.
    pub fn status_code(&self) -> u16 {
        400
    }

    /// Whether the refusal is the request's alone: always, so the credential
    /// stays usable.
    pub fn is_request_scoped(&self) -> bool {
        true
    }
}

impl fmt::Display for UnsupportedPartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.part_type.is_empty() {
            f.write_str("unsupported content part")
        } else {
            write!(f, "unsupported content part: {}", self.part_type)
        }
    }
}

impl Error for UnsupportedPartError {}

/// Which user turn a translation emptied. Text or any other sendable part
/// beside a dropped one still goes out; a user turn left with nothing to send
/// is refused. The decision is made per turn, so other turns and system or
/// developer prompts can't hide an emptied one.
#[derive(Clone, Debug, Default)]
pub(crate) struct UserTurnDrops {
    turn: String,
    first: String,
}

impl UserTurnDrops {
    /// Records a part of the current user turn that can't be sent. The first
    /// type recorded within a turn is the one reported.
    pub(crate) fn drop_part(&mut self, part_type: &str) {
        if self.turn.is_empty() {
            self.turn = part_type.to_owned();
        }
    }

    /// Closes the current user turn, which contributed `sendable` parts to
    /// the translated request. A turn that dropped a part and contributed
    /// nothing is remembered; a later turn never clears it.
    pub(crate) fn end_turn(&mut self, sendable: usize) {
        if !self.turn.is_empty() && sendable == 0 && self.first.is_empty() {
            self.first = std::mem::take(&mut self.turn);
        }
        self.turn.clear();
    }

    /// The refusal for the first emptied user turn.
    pub(crate) fn err(&self) -> Option<UnsupportedPartError> {
        (!self.first.is_empty()).then(|| UnsupportedPartError::new(self.first.clone()))
    }
}

/// Follows the consecutive user content that a target merges into one user
/// turn. Callers pass `None` for content that isn't the user's, which makes
/// every method a no-op (upstream's nil receiver).
#[derive(Clone, Debug, Default)]
pub(crate) struct UserRun {
    drops: UserTurnDrops,
    sendable: usize,
}

impl UserRun {
    /// Records one sendable item in the current user turn.
    pub(crate) fn add(&mut self) {
        self.sendable += 1;
    }

    /// Records a part of the current user turn that can't be sent.
    pub(crate) fn drop_part(&mut self, part_type: &str) {
        self.drops.drop_part(part_type);
    }

    /// Closes the current user turn.
    pub(crate) fn end(&mut self) {
        self.drops.end_turn(self.sendable);
        self.sendable = 0;
    }

    /// The refusal for the first emptied user turn.
    pub(crate) fn err(&self) -> Option<UnsupportedPartError> {
        self.drops.err()
    }
}

/// Whether an Interactions step carries developer or system content rather
/// than user content. The step's `role` decides, then its `type`; a step that
/// names neither takes `inherited`, the answer for the wrapper it sits in.
pub(crate) fn is_interactions_instruction_step(step: &Value, inherited: bool) -> bool {
    let mut name = str_of(step.get("role")).trim().to_lowercase();
    if name.is_empty() {
        name = str_of(step.get("type")).trim().to_lowercase();
    }
    match name.as_str() {
        "developer" | "system" => true,
        "user" | "assistant" | "model" | "model_output" | "thought" => false,
        _ => inherited,
    }
}

/// The type of an Interactions content part that carries an attachment, or
/// `""` for text and for parts that name nothing. It's the type reported
/// when a translation can't send the part.
pub(crate) fn interactions_attachment_type(part: &Value) -> String {
    let Some(object) = part.as_object() else {
        return String::new();
    };
    let part_type = str_of(object.get("type")).trim().to_lowercase();
    if !part_type.is_empty() {
        return if part_type == "text" {
            String::new()
        } else {
            part_type
        };
    }
    if object.contains_key("inlineData") || object.contains_key("inline_data") {
        return "inlineData".to_owned();
    }
    if object.contains_key("fileData") || object.contains_key("file_data") {
        return "fileData".to_owned();
    }
    String::new()
}

/// Whether a Gemini part gives the model something to read. A text part
/// that is empty or only whitespace doesn't.
pub(crate) fn gemini_part_is_sendable(part: &Value) -> bool {
    let Some(text) = path(part, "text") else {
        return true;
    };
    if !str_of(Some(text)).trim().is_empty() {
        return true;
    }
    [
        "functionCall",
        "functionResponse",
        "inlineData",
        "inline_data",
        "fileData",
        "file_data",
    ]
    .into_iter()
    .any(|key| path(part, key).is_some())
}

/// How many of `parts` [`gemini_part_is_sendable`] holds for.
pub(crate) fn count_sendable_gemini_parts<'a>(parts: impl IntoIterator<Item = &'a Value>) -> usize {
    parts
        .into_iter()
        .filter(|part| gemini_part_is_sendable(part))
        .count()
}

/// Whether `value` is an absolute `http` or `https` URL with a host.
pub(crate) fn is_http_url(value: &str) -> bool {
    let value = value.trim();
    // Go's url.Parse refuses any ASCII control character.
    if value.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return false;
    }
    let Some((scheme, rest)) = value.split_once(':') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let Some(rest) = rest.strip_prefix("//") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    valid_host(host)
}

/// Whether Go's `url.Parse` accepts `host`, and it isn't empty.
fn valid_host(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    let port = if let Some(bracketed) = host.strip_prefix('[') {
        let Some((_, after)) = bracketed.split_once(']') else {
            return false;
        };
        if after.is_empty() {
            None
        } else {
            match after.strip_prefix(':') {
                Some(port) => Some(port),
                None => return false,
            }
        }
    } else {
        if host.bytes().any(|b| b < 0x80 && host_byte_needs_escape(b)) {
            return false;
        }
        host.rsplit_once(':').map(|(_, port)| port)
    };
    port.is_none_or(|port| port.bytes().all(|b| b.is_ascii_digit()))
}

/// Go's `shouldEscape(c, encodeHost)` for an ASCII byte.
fn host_byte_needs_escape(b: u8) -> bool {
    !(b.is_ascii_alphanumeric() || b"-_.~!$&'()*+,;=:[]<>\"%".contains(&b))
}

#[cfg(test)]
mod tests;
