// Ported from CLIProxyAPI internal/translator/common/apply_patch_input.go and
// apply_patch_events.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Decoding `apply_patch` function arguments as they stream in.
//!
//! A provider that only knows function tools streams the arguments
//! `{"input": "<patch>"}` a fragment at a time. A Responses client expects the
//! custom tool's raw input instead, so [`InputDecoder`] reads the patch text
//! out of the fragments and hands back each newly complete piece. It accepts
//! only that one object, and only characters that are valid as they arrive:
//! unlike Go's `encoding/json`, it rejects an unpaired surrogate escape or
//! invalid UTF-8 rather than writing U+FFFD.

use std::fmt;

use serde_json::{Value, json};

use super::{decode_string, unwrap_input};

/// Why `apply_patch` arguments were rejected. Errors are kept from the client,
/// which gets only [`failure`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InputError(&'static str);

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InputError {}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    BeforeObject,
    BeforeKey,
    InKey,
    BeforeColon,
    BeforeValue,
    InValue,
    AfterValue,
    Complete,
}

/// `ApplyPatchInputDecoder`: decodes one call's `input` string from its
/// streamed arguments.
#[derive(Default)]
pub(crate) struct InputDecoder {
    phase: Phase,
    /// The key so far, quotes and escapes included.
    key_raw: Vec<u8>,
    /// An escape in progress: the backslash and what follows it.
    escape: Vec<u8>,
    /// The bytes so far of a UTF-8 character split across fragments.
    utf8_pending: Vec<u8>,
    /// A high surrogate escape waiting for its low surrogate, or 0.
    high_surrogate: u16,
    input: String,
    finished: bool,
    error: Option<InputError>,
}

impl InputDecoder {
    /// `Push`: reads the next fragment of the arguments and returns the
    /// characters it completed. A fragment may end partway through an escape
    /// or a UTF-8 character; the rest waits for the next one.
    pub(crate) fn push(&mut self, fragment: impl AsRef<[u8]>) -> Result<String, InputError> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let fragment = fragment.as_ref();
        if self.finished {
            if fragment.is_empty() {
                return Ok(String::new());
            }
            return Err(self.fail("apply_patch arguments received after completion"));
        }
        let start = self.input.len();
        for &c in fragment {
            if let Err(message) = self.consume(c) {
                return Err(self.fail(message));
            }
        }
        Ok(self.input[start..].to_owned())
    }

    fn consume(&mut self, c: u8) -> Result<(), &'static str> {
        match self.phase {
            Phase::BeforeObject => {
                if !is_space(c) {
                    if c != b'{' {
                        return Err("apply_patch arguments must be a JSON object");
                    }
                    self.phase = Phase::BeforeKey;
                }
            }
            Phase::BeforeKey => {
                if !is_space(c) {
                    if c != b'"' {
                        return Err("apply_patch arguments must contain the input field");
                    }
                    self.key_raw.push(c);
                    self.phase = Phase::InKey;
                }
            }
            Phase::InKey => {
                self.key_raw.push(c);
                if !self.escape.is_empty() {
                    self.escape.clear();
                } else if c == b'\\' {
                    self.escape.push(c);
                } else if c < 0x20 {
                    return Err("invalid control character in apply_patch input key");
                } else if c == b'"' {
                    let key = std::str::from_utf8(&self.key_raw)
                        .ok()
                        .and_then(decode_string)
                        .ok_or("decode apply_patch input key")?;
                    if key != "input" {
                        return Err("apply_patch arguments must contain the input field");
                    }
                    self.key_raw.clear();
                    self.phase = Phase::BeforeColon;
                }
            }
            Phase::BeforeColon => {
                if !is_space(c) {
                    if c != b':' {
                        return Err("apply_patch input key must be followed by a colon");
                    }
                    self.phase = Phase::BeforeValue;
                }
            }
            Phase::BeforeValue => {
                if !is_space(c) {
                    if c != b'"' {
                        return Err("apply_patch input must be a string");
                    }
                    self.phase = Phase::InValue;
                }
            }
            Phase::InValue => self.consume_value(c)?,
            Phase::AfterValue => {
                if !is_space(c) {
                    if c != b'}' {
                        return Err("apply_patch arguments must contain only one input field");
                    }
                    self.phase = Phase::Complete;
                }
            }
            Phase::Complete => {
                if !is_space(c) {
                    return Err("apply_patch arguments must not contain trailing JSON");
                }
            }
        }
        Ok(())
    }

    fn consume_value(&mut self, c: u8) -> Result<(), &'static str> {
        if !self.utf8_pending.is_empty() || !c.is_ascii() {
            // Pending escapes and surrogate pairs cannot consume raw UTF-8.
            if !self.escape.is_empty() || self.high_surrogate != 0 {
                return Err("invalid Unicode escape in apply_patch input");
            }
            self.utf8_pending.push(c);
            match std::str::from_utf8(&self.utf8_pending) {
                Ok(character) => {
                    self.input.push_str(character);
                    self.utf8_pending.clear();
                }
                // The character isn't complete yet.
                Err(error) if error.error_len().is_none() => {}
                Err(_) => return Err("invalid UTF-8 in apply_patch input"),
            }
            return Ok(());
        }
        if !self.escape.is_empty() {
            self.escape.push(c);
            if self.escape.len() == 2 {
                if self.high_surrogate != 0 && c != b'u' {
                    return Err("apply_patch input high surrogate requires a low surrogate");
                }
                let decoded = match c {
                    b'u' => return Ok(()),
                    b'"' | b'\\' | b'/' => char::from(c),
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    _ => return Err("invalid escape in apply_patch input"),
                };
                self.input.push(decoded);
                self.escape.clear();
                return Ok(());
            }
            if !c.is_ascii_hexdigit() {
                return Err("invalid Unicode escape in apply_patch input");
            }
            if self.escape.len() < 6 {
                return Ok(());
            }
            let code = self.escape[2..].iter().fold(0u16, |code, &digit| {
                code << 4 | char::from(digit).to_digit(16).unwrap_or(0) as u16
            });
            self.escape.clear();
            if self.high_surrogate != 0 {
                if !(0xdc00..=0xdfff).contains(&code) {
                    return Err("apply_patch input high surrogate requires a low surrogate");
                }
                let high = u32::from(self.high_surrogate - 0xd800);
                let low = u32::from(code - 0xdc00);
                self.input
                    .push(char::from_u32(0x10000 + (high << 10) + low).expect("a pair"));
                self.high_surrogate = 0;
                return Ok(());
            }
            match code {
                0xd800..=0xdbff => self.high_surrogate = code,
                0xdc00..=0xdfff => return Err("unpaired low surrogate in apply_patch input"),
                _ => self
                    .input
                    .push(char::from_u32(code.into()).expect("not a surrogate")),
            }
            return Ok(());
        }
        if self.high_surrogate != 0 && c != b'\\' {
            return Err("apply_patch input high surrogate requires a low surrogate");
        }
        match c {
            b'\\' => self.escape.push(c),
            b'"' => self.phase = Phase::AfterValue,
            0..0x20 => return Err("invalid control character in apply_patch input"),
            _ => self.input.push(char::from(c)),
        }
        Ok(())
    }

    /// `Finish`: checks the complete arguments against what streamed in and
    /// returns the part of the input not yet returned. Once finished, the same
    /// arguments, however they're encoded, finish it again with nothing new.
    pub(crate) fn finish(&mut self, arguments: impl AsRef<[u8]>) -> Result<String, InputError> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let arguments = arguments.as_ref();
        let Some(input) = std::str::from_utf8(arguments).ok().and_then(unwrap_input) else {
            return Err(self.fail("apply_patch arguments must be one input string"));
        };
        // Go's `encoding/json` replaces invalid UTF-8 and unpaired surrogates.
        // The final arguments need the same strict check as streamed fragments.
        if let Err(error) = InputDecoder::default().push(arguments) {
            self.error = Some(error.clone());
            return Err(error);
        }
        if self.finished {
            if input != self.input {
                return Err(self.fail("conflicting apply_patch arguments completion"));
            }
            return Ok(String::new());
        }
        let Some(tail) = input.strip_prefix(self.input.as_str()) else {
            return Err(self.fail("final apply_patch input conflicts with streamed input"));
        };
        let tail = tail.to_owned();
        self.input.push_str(&tail);
        self.finished = true;
        self.phase = Phase::Complete;
        self.key_raw.clear();
        self.escape.clear();
        self.utf8_pending.clear();
        self.high_surrogate = 0;
        Ok(tail)
    }

    /// The input decoded so far, whitespace and all.
    pub(crate) fn input(&self) -> &str {
        &self.input
    }

    fn fail(&mut self, message: &'static str) -> InputError {
        let error = InputError(message);
        self.error = Some(error.clone());
        error
    }
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n')
}

/// `ApplyPatchCallState`: one `apply_patch` call, its identity in the
/// Responses stream and its input decoder.
#[derive(Default)]
pub(crate) struct CallState {
    pub(crate) item_id: String,
    pub(crate) call_id: String,
    pub(crate) output_index: i64,
    decoder: InputDecoder,
}

impl CallState {
    pub(crate) fn new(item_id: String, call_id: String, output_index: i64) -> Self {
        Self {
            item_id,
            call_id,
            output_index,
            decoder: InputDecoder::default(),
        }
    }

    /// `PushArguments`: see [`InputDecoder::push`].
    pub(crate) fn push_arguments(
        &mut self,
        fragment: impl AsRef<[u8]>,
    ) -> Result<String, InputError> {
        self.decoder.push(fragment)
    }

    /// `FinishArguments`: the part of the input not yet returned, and the
    /// whole input.
    pub(crate) fn finish_arguments(
        &mut self,
        arguments: impl AsRef<[u8]>,
    ) -> Result<(String, String), InputError> {
        let tail = self.decoder.finish(arguments)?;
        Ok((tail, self.decoder.input().to_owned()))
    }

    /// The input decoded so far.
    pub(crate) fn input(&self) -> &str {
        self.decoder.input()
    }

    /// `ApplyPatchInputDelta`: a `response.custom_tool_call_input.delta`
    /// event, without SSE framing.
    pub(crate) fn input_delta(&self, delta: &str, sequence: i64) -> Value {
        json!({
            "type": "response.custom_tool_call_input.delta",
            "item_id": self.item_id,
            "call_id": self.call_id,
            "output_index": self.output_index,
            "sequence_number": sequence,
            "delta": delta,
        })
    }

    /// `ApplyPatchInputDone`: a `response.custom_tool_call_input.done` event,
    /// without SSE framing.
    pub(crate) fn input_done(&self, input: &str, sequence: i64) -> Value {
        json!({
            "type": "response.custom_tool_call_input.done",
            "item_id": self.item_id,
            "call_id": self.call_id,
            "output_index": self.output_index,
            "sequence_number": sequence,
            "input": input,
        })
    }
}

/// `ApplyPatchFailure`: the `response.failed` event that ends a stream whose
/// `apply_patch` arguments were invalid. It says nothing about the arguments.
pub(crate) fn failure(response_id: &str, sequence: i64) -> Value {
    json!({
        "type": "response.failed",
        "sequence_number": sequence,
        "response": {
            "id": response_id,
            "object": "response",
            "status": "failed",
            "error": {
                "type": "server_error",
                "code": "invalid_tool_arguments",
                "message": "Invalid apply_patch tool arguments received from upstream.",
                "param": null,
            },
        },
    })
}

#[cfg(test)]
mod tests;
