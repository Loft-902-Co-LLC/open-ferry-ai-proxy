// Ported from responsesSSEFramer and its helpers in CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_handlers.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Puts a Responses event stream back into whole events as it passes, drops
//! private events, turns error payloads into one terminal failure, and fills
//! in a `response.completed` output the provider left empty.
//!
//! Deviations from upstream:
//! - The held-back event is read on from where the last chunk left it, by
//!   an [`EventScan`], and whole events are written from where they lie, so
//!   a stream costs time in step with its size. Upstream reads the held-back
//!   event again, and copies what is left, for each chunk.
//! - The held-back event may grow to at most [`MAX_EVENT_BYTES`]. Past it,
//!   [`Framer::write_chunk`] drops it and fails with a 502, which ends the
//!   stream. Upstream holds it back without limit.
//!
//! [`MAX_EVENT_BYTES`]: crate::sse_check::MAX_EVENT_BYTES

use std::borrow::Cow;
use std::collections::HashMap;

use bytes::BytesMut;
use open_ferry_translate::go;

use super::stream_error::{
    error_chunk, failed_chunk, sanitize_error, sanitize_event_name, stream_error_text,
};
use crate::errors::ErrorMessage;
use crate::json;
use crate::sse_check::{EventLimit, EventScan, Work, data_payload, too_large};

/// Where a Responses stream is (`responsesSSEFramer`).
#[derive(Debug, Default)]
pub(super) struct Framer {
    /// The event held back.
    pending: Vec<u8>,
    /// Where a blank line ending the held-back event may start: no sooner
    /// than three bytes before the end of the last chunk.
    search: usize,
    /// What the held-back event says so far.
    scan: EventScan,
    limit: EventLimit,
    work: Work,
    output_items: HashMap<i64, Vec<u8>>,
    output_order: Vec<i64>,
    unindexed_output_items: Vec<Vec<u8>>,
    /// The last event's name, made safe to log.
    pub(super) last_event: String,
    /// The event that ended the stream, or empty.
    pub(super) terminal_event: String,
    /// The error a payload ended the stream with.
    pub(super) terminal_error: Option<ErrorMessage>,
    failure_event: &'static str,
    /// Whether the client is an official Codex client.
    pub(super) is_codex_client: bool,
    /// How many data payloads have gone to the client.
    pub(super) data_frames: i64,
}

impl Framer {
    /// A framer for a Codex client, which gets `response.failed` for a
    /// failure, or another client, which gets `error`.
    pub(super) fn new(is_codex_client: bool) -> Self {
        Self {
            failure_event: if is_codex_client {
                "response.failed"
            } else {
                "error"
            },
            is_codex_client,
            ..Self::default()
        }
    }

    /// This framer, holding back at most `limit` bytes of an event.
    #[cfg(test)]
    pub(super) fn with_limit(mut self, limit: usize) -> Self {
        self.limit = EventLimit(limit);
        self
    }

    /// How many bytes have been read in looking for whole events.
    #[cfg(test)]
    pub(super) fn work(&self) -> usize {
        self.work.0
    }

    /// How many bytes are held back.
    #[cfg(test)]
    pub(super) fn held(&self) -> usize {
        self.pending.len()
    }

    /// Adds a chunk of the stream, writing the events it completes
    /// (`WriteChunk`). Fails, holding nothing back, when the held-back event
    /// grows past the limit.
    pub(super) fn write_chunk(
        &mut self,
        out: &mut BytesMut,
        chunk: &[u8],
    ) -> Result<(), ErrorMessage> {
        if chunk.is_empty() || !self.terminal_event.is_empty() {
            return Ok(());
        }
        if starts_new_data_frame(&self.scan, chunk) {
            let frame = self.take_pending();
            self.write_frame(out, &frame);
            if !self.terminal_event.is_empty() {
                return Ok(());
            }
        }
        if needs_line_break(&self.pending, chunk) {
            self.pending.push(b'\n');
        }
        self.pending.extend_from_slice(chunk);
        // Each whole event is written from where it lies, and what is left is
        // moved down once.
        let mut pending = std::mem::take(&mut self.pending);
        let mut start = 0;
        while let Some(end) = frame_end(&pending, self.search.max(start), &mut self.work) {
            self.write_frame(out, pending.get(start..end).unwrap_or_default());
            start = end;
            if !self.terminal_event.is_empty() {
                self.reset();
                return Ok(());
            }
        }
        if start > 0 {
            pending.drain(..start);
            self.scan = EventScan::default();
        }
        self.search = pending.len().saturating_sub(3);
        self.pending = pending;
        self.work.add(self.scan.advance(&self.pending));
        if self.scan.is_blank() {
            self.reset();
            return Ok(());
        }
        if self.pending.is_empty() || !can_emit_without_delimiter(&self.scan) {
            if self.pending.len() > self.limit.0 {
                self.reset();
                return Err(ErrorMessage::new(502, too_large(self.limit)));
            }
            return Ok(());
        }
        let frame = self.take_pending();
        self.write_frame(out, &frame);
        Ok(())
    }

    /// Writes what is held back when the stream ends, if it is a whole
    /// event, and drops it otherwise (`Flush`).
    pub(super) fn flush(&mut self, out: &mut BytesMut) {
        if self.pending.is_empty() || !self.terminal_event.is_empty() {
            return;
        }
        if self.scan.is_blank() || !can_flush_without_delimiter(&self.scan) {
            self.reset();
            return;
        }
        let frame = self.take_pending();
        self.write_frame(out, &frame);
    }

    /// Takes what is held back.
    fn take_pending(&mut self) -> Vec<u8> {
        let pending = std::mem::take(&mut self.pending);
        self.reset();
        pending
    }

    /// Drops what is held back.
    fn reset(&mut self) {
        self.pending.clear();
        self.search = 0;
        self.scan = EventScan::default();
    }

    /// `writeFrame`.
    fn write_frame(&mut self, out: &mut BytesMut, frame: &[u8]) {
        let frame = self.repair_frame(frame);
        write_sse_chunk(out, &frame);
    }

    /// Whether an event is private to the provider
    /// (`shouldFilterPrivateEvent`). Codex clients keep Codex events, all
    /// but `codex.rate_limits`.
    fn should_filter_private_event(&self, stream_event: &str, payload_type: &str) -> bool {
        let check = |name: &str| {
            let name = name.trim();
            if name.is_empty() || is_error_event(name) {
                return false;
            }
            if name.starts_with("responsesapi.") {
                return true;
            }
            if self.is_codex_client {
                return name == "codex.rate_limits";
            }
            name.starts_with("codex.")
        };
        check(stream_event) || check(payload_type)
    }

    /// The event as the client gets it, or nothing (`repairFrame`).
    fn repair_frame<'f>(&mut self, frame: &'f [u8]) -> Cow<'f, [u8]> {
        let payload = data_payload(frame);
        let stream_event = event_name(frame);
        if !stream_event.is_empty() && self.should_filter_private_event(&stream_event, "") {
            return Cow::Borrowed(&[]);
        }
        let Some(payload) = payload.filter(|payload| !payload.is_empty()) else {
            return Cow::Borrowed(frame);
        };
        if payload == b"[DONE]" {
            self.data_frames += 1;
            return Cow::Borrowed(frame);
        }
        if !go::json_valid(&payload) {
            return Cow::Borrowed(frame);
        }

        let payload_type = json::str_at(&payload, "type");
        if self.should_filter_private_event(&stream_event, &payload_type) {
            return Cow::Borrowed(&[]);
        }

        self.data_frames += 1;

        if is_error_event(&payload_type) || payload_has_error(&payload) {
            if !payload_type.is_empty() {
                self.last_event = sanitize_event_name(&payload_type);
            }
            return Cow::Owned(self.repair_error_payload(&payload));
        }
        let event_type = if is_terminal_event(&stream_event) || payload_type.is_empty() {
            stream_event
        } else {
            payload_type
        };
        if !event_type.is_empty() {
            self.last_event = sanitize_event_name(&event_type);
        }
        if is_error_event(&event_type) {
            return Cow::Owned(self.repair_error_payload(&payload));
        }
        if is_terminal_event(&event_type) {
            self.terminal_event.clone_from(&event_type);
        }

        match event_type.as_str() {
            "response.output_item.done" => self.record_output_item(&payload),
            "response.completed" => {
                if let Some(repaired) = self.repair_completed_payload(&payload) {
                    return Cow::Owned(frame_with_data(frame, &repaired));
                }
            }
            _ => {}
        }
        Cow::Borrowed(frame)
    }

    /// Ends the stream with the error a payload carries, written as the
    /// client's failure event (`repairErrorPayload`).
    fn repair_error_payload(&mut self, payload: &[u8]) -> Vec<u8> {
        let error = payload_error_message(payload);
        let status = error.status;
        let err_text = stream_error_text(&error, status);
        self.terminal_error = Some(error);
        let failure_event = if self.failure_event == "response.failed" {
            "response.failed"
        } else {
            "error"
        };
        failure_event.clone_into(&mut self.terminal_event);
        let sequence = if let Some(sequence) = json::get(payload, "sequence_number") {
            sequence.int()
        } else if let Some(sequence) = json::find(err_text.as_bytes(), "sequence_number") {
            sequence.int()
        } else if self.data_frames > 0 {
            self.data_frames - 1
        } else {
            0
        };
        let chunk = if failure_event == "response.failed" {
            failed_chunk(status, &err_text, sequence)
        } else {
            error_chunk(status, &err_text, sequence)
        };
        format!("event: {failure_event}\ndata: {chunk}\n\n").into_bytes()
    }

    /// Keeps a finished output item for [`Framer::repair_completed_payload`]
    /// (`recordOutputItem`).
    fn record_output_item(&mut self, payload: &[u8]) {
        let Some(item) = json::get(payload, "item") else {
            return;
        };
        if !item.is_object()
            || item
                .get("type")
                .map(|kind| kind.str())
                .unwrap_or_default()
                .is_empty()
        {
            return;
        }
        if let Some(index) = json::get(payload, "output_index") {
            let index = index.int();
            if !self.output_items.contains_key(&index) {
                self.output_order.push(index);
            }
            self.output_items.insert(index, item.raw.to_vec());
            return;
        }
        self.unindexed_output_items.push(item.raw.to_vec());
    }

    /// A `response.completed` payload with the finished items as its output,
    /// when it has none (`repairCompletedPayload`).
    fn repair_completed_payload(&self, payload: &[u8]) -> Option<Vec<u8>> {
        if self.output_order.is_empty() && self.unindexed_output_items.is_empty() {
            return None;
        }
        if json::get(payload, "response.output").is_some_and(|output| !output.is_empty_array()) {
            return None;
        }
        let mut indexes = self.output_order.clone();
        indexes.sort_unstable();
        let items = indexes
            .iter()
            .filter_map(|index| self.output_items.get(index))
            .chain(&self.unindexed_output_items);
        let mut output = vec![b'['];
        for (i, item) in items.enumerate() {
            if i > 0 {
                output.push(b',');
            }
            output.extend_from_slice(item);
        }
        output.push(b']');
        json::try_set_raw(payload, "response.output", &output)
            .filter(|repaired| repaired != payload)
    }
}

/// Writes an event, ending it with a blank line if it lacks one
/// (`writeResponsesSSEChunk`).
fn write_sse_chunk(out: &mut BytesMut, chunk: &[u8]) {
    if chunk.is_empty() {
        return;
    }
    out.extend_from_slice(chunk);
    if chunk.ends_with(b"\n\n") || chunk.ends_with(b"\r\n\r\n") {
        return;
    }
    let suffix: &[u8] = if chunk.ends_with(b"\r\n") {
        b"\r\n"
    } else if chunk.ends_with(b"\n") {
        b"\n"
    } else {
        b"\n\n"
    };
    out.extend_from_slice(suffix);
}

/// The error a payload carries, with the first status from 400 to 599 it
/// gives, otherwise 502 (`responsesSSEPayloadErrorMessage`).
fn payload_error_message(payload: &[u8]) -> ErrorMessage {
    let paths = [
        "status",
        "status_code",
        "error.status",
        "error.status_code",
        "response.error.status",
        "response.error.status_code",
    ];
    let status = paths
        .iter()
        .map(|path| json::get(payload, path).map_or(0, |status| status.int()))
        .find(|status| (400..=599).contains(status))
        .map_or(502, |status| status as u16);
    sanitize_error(ErrorMessage::new(
        status,
        String::from_utf8_lossy(payload).into_owned(),
    ))
}

/// `responsesSSEErrorEvent`.
fn is_error_event(event: &str) -> bool {
    matches!(event, "response.failed" | "response.error" | "error")
}

/// `responsesSSETerminalEvent`.
fn is_terminal_event(event: &str) -> bool {
    matches!(
        event,
        "response.completed"
            | "response.incomplete"
            | "response.failed"
            | "response.done"
            | "response.error"
            | "error"
    )
}

/// Whether a payload carries an error (`responsesSSEPayloadHasError`).
fn payload_has_error(payload: &[u8]) -> bool {
    let has_error = ["error", "response.error"]
        .iter()
        .any(|path| json::get(payload, path).is_some_and(|error| error.raw != b"null"));
    has_error || (json::get(payload, "code").is_some() && json::get(payload, "message").is_some())
}

/// An event with its data lines replaced by `payload`'s lines
/// (`responsesSSEFrameWithData`).
fn frame_with_data(frame: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in frame.split(|&b| b == b'\n') {
        let line = trim_trailing_cr(line);
        let trimmed = go::trim_space(line);
        if trimmed.is_empty() || trimmed.starts_with(b"data:") {
            continue;
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    for line in payload.split(|&b| b == b'\n') {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.push(b'\n');
    out
}

/// `line` without the carriage returns at its end.
fn trim_trailing_cr(line: &[u8]) -> &[u8] {
    let end = line.iter().rposition(|&b| b != b'\r').map_or(0, |i| i + 1);
    &line[..end]
}

/// Where the first whole event in `pending` ends, past the blank line that
/// ends it, looking for that line from `from` (`responsesSSEFrameLen`, which
/// looks from the start).
fn frame_end(pending: &[u8], from: usize, work: &mut Work) -> Option<usize> {
    let rest = pending.get(from..).unwrap_or_default();
    let found = (0..rest.len()).find_map(|at| {
        let tail = rest.get(at..).unwrap_or_default();
        if tail.starts_with(b"\n\n") {
            Some(at + 2)
        } else if tail.starts_with(b"\r\n\r\n") {
            Some(at + 4)
        } else {
            None
        }
    });
    work.add(found.unwrap_or(rest.len()));
    Some(from + found?)
}

/// Whether the held-back event may go before its blank line: it has its
/// name and whole data (`responsesSSECanEmitWithoutDelimiter`, whose
/// `responsesSSENeedsMoreData` check the name and data checks cover).
fn can_emit_without_delimiter(scan: &EventScan) -> bool {
    !scan.is_blank() && scan.has_event() && scan.has_data() && scan.data_valid()
}

/// Whether what is held back at the end is an event with whole data
/// (`responsesSSECanFlushWithoutDelimiter`).
fn can_flush_without_delimiter(scan: &EventScan) -> bool {
    !scan.is_blank() && scan.has_data() && scan.data_valid()
}

/// Whether `chunk` starts the next event after a held-back event of whole
/// data with no name (`responsesSSEStartsNewDataFrame`).
fn starts_new_data_frame(scan: &EventScan, chunk: &[u8]) -> bool {
    if scan.is_blank() || scan.has_event() || !scan.has_data() || !scan.data_valid() {
        return false;
    }
    let start = chunk
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(chunk.len());
    chunk[start..].starts_with(b"data:")
}

/// The event's name, or empty (`responsesSSEEventName`).
fn event_name(frame: &[u8]) -> String {
    frame
        .split(|&b| b == b'\n')
        .find_map(|line| go::trim_space(line).strip_prefix(b"event:"))
        .map(|name| String::from_utf8_lossy(name).trim().to_owned())
        .unwrap_or_default()
}

/// Whether a newline has to go between held-back text and a chunk that
/// starts a field (`responsesSSENeedsLineBreak`).
fn needs_line_break(pending: &[u8], chunk: &[u8]) -> bool {
    if pending.is_empty() || chunk.is_empty() {
        return false;
    }
    if pending.ends_with(b"\n") || pending.ends_with(b"\r") {
        return false;
    }
    if chunk[0] == b'\n' || chunk[0] == b'\r' {
        return false;
    }
    let start = chunk
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t'))
        .unwrap_or(chunk.len());
    let trimmed = &chunk[start..];
    !trimmed.is_empty()
        && [&b"data:"[..], b"event:", b"id:", b"retry:", b":"]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(framer: &mut Framer, chunks: &[&str]) -> String {
        let mut out = BytesMut::new();
        for chunk in chunks {
            framer.write_chunk(&mut out, chunk.as_bytes()).unwrap();
        }
        String::from_utf8(out.to_vec()).unwrap()
    }

    const METADATA: &str = "event: codex.response.metadata\ndata: {\"type\":\"codex.response.metadata\",\"headers\":{\"x-turn-state\":\"turn-1\"}}\n\n";
    const RATE_LIMITS: &str = "event: codex.rate_limits\ndata: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"primary\":{\"used_percent\":42}}}\n\n";
    const TIMING: &str = "event: responsesapi.websocket_timing\ndata: {\"type\":\"responsesapi.websocket_timing\",\"timing\":{\"duration_ms\":100}}\n\n";
    const CREATED: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-1\"}}\n\n";
    const COMPLETED: &str = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"status\":\"completed\"}}\n\n";

    // TestResponsesSSEFramerWaitsForEventFieldAfterData
    #[test]
    fn waits_for_event_field_after_data() {
        let mut framer = Framer::new(false);
        let data = r#"data: {"response":{"id":"resp-1","status":"completed"}}"#;
        assert_eq!(write(&mut framer, &[data]), "");
        let out = write(&mut framer, &["event: response.completed"]);
        assert_eq!(framer.terminal_event, "response.completed");
        assert_eq!(
            out,
            "data: {\"response\":{\"id\":\"resp-1\",\"status\":\"completed\"}}\nevent: response.completed\n\n"
        );
    }

    // TestResponsesSSEFramerFlushesMultilineDataWithoutDelimiter
    #[test]
    fn flushes_multiline_data_without_delimiter() {
        let mut framer = Framer::new(false);
        let chunk = "event: response.completed\n\
                     data: {\"type\":\"response.completed\",\n\
                     data: \"response\":{\"id\":\"resp-1\",\"status\":\"completed\"}}";
        let mut out = BytesMut::new();
        framer.write_chunk(&mut out, chunk.as_bytes()).unwrap();
        framer.flush(&mut out);
        assert_eq!(framer.terminal_event, "response.completed");
        assert_eq!(&out[..], format!("{chunk}\n\n").as_bytes());
    }

    // TestResponsesSSEFramerUsesPayloadErrorOverCompletedEvent
    #[test]
    fn uses_payload_error_over_completed_event() {
        let mut framer = Framer::new(false);
        framer.failure_event = "response.failed";
        let out = write(
            &mut framer,
            &[
                "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\"}}\nevent: response.completed\n\n",
            ],
        );
        assert_eq!(framer.terminal_event, "response.failed");
        assert_eq!(
            out,
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":0,\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"internal_server_error\",\"message\":\"{\\\"response\\\":{\\\"status\\\":\\\"failed\\\"},\\\"type\\\":\\\"response.failed\\\"}\",\"param\":null,\"type\":\"response.failed\"}}}\n\n"
        );
    }

    // TestResponsesSSEFramerUsesErrorEventOverPayloadType
    #[test]
    fn uses_error_event_over_payload_type() {
        let mut framer = Framer::new(false);
        write(
            &mut framer,
            &["event: error\ndata: {\"type\":\"provider.error\",\"message\":\"failed\"}\n\n"],
        );
        assert_eq!(framer.terminal_event, "error");

        let mut framer = Framer::new(false);
        write(
            &mut framer,
            &["data: {\"response\":{\"error\":{\"message\":\"failed\"}}}\n\n"],
        );
        assert_eq!(framer.terminal_event, "error");
    }

    // TestResponsesSSENeedsLineBreakSkipsChunksThatAlreadyStartWithNewline
    #[test]
    fn needs_line_break_skips_chunks_that_start_with_a_newline() {
        assert!(!needs_line_break(b"event: response.created", b"\n"));
        assert!(!needs_line_break(b"event: response.created", b"\r\n"));
        assert!(needs_line_break(b"event: response.created", b"  data: {}"));
        assert!(!needs_line_break(b"data: {\"a\":", b"1}"));
        // A leading `:` reads as an SSE comment, as upstream.
        assert!(needs_line_break(b"data: {\"a\"", b":1}"));
    }

    // TestResponsesSSEFramerRepairErrorPayloadCalculatesCorrectSequenceNumber
    #[test]
    fn repair_error_payload_calculates_sequence_number() {
        let mut first = Framer::new(false);
        let out = write(
            &mut first,
            &["event: error\ndata: {\"error\":{\"code\":\"bad_request\"}}\n\n"],
        );
        assert_eq!(
            out,
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"bad_request\"},\"sequence_number\":0}\n\n"
        );

        let mut third = Framer::new(false);
        write(
            &mut third,
            &[
                "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
                "event: response.in_progress\ndata: {\"type\":\"response.in_progress\",\"sequence_number\":1}\n\n",
            ],
        );
        let out = write(
            &mut third,
            &["event: error\ndata: {\"error\":{\"code\":\"cyber_policy\"}}\n\n"],
        );
        assert_eq!(
            out,
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"cyber_policy\"},\"sequence_number\":2}\n\n"
        );
    }

    // TestResponsesSSEFramer_FiltersUpstreamPrivateEvents
    #[test]
    fn filters_upstream_private_events() {
        let mut framer = Framer::new(false);
        let out = write(
            &mut framer,
            &[RATE_LIMITS, METADATA, CREATED, TIMING, COMPLETED],
        );
        assert_eq!(out, format!("{CREATED}{COMPLETED}"));
    }

    // TestResponsesSSEFramer_PreservesMetadataForCodexClient
    #[test]
    fn preserves_metadata_for_codex_client() {
        let mut framer = Framer::new(true);
        let out = write(&mut framer, &[METADATA, CREATED, COMPLETED]);
        assert_eq!(out, format!("{METADATA}{CREATED}{COMPLETED}"));
    }

    // TestResponsesSSEFramer_FiltersDataOnlyPrivateEventsAndNonJSON
    #[test]
    fn filters_data_only_private_events_and_non_json() {
        let mut framer = Framer::new(false);
        let out = write(
            &mut framer,
            &[
                "data: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"primary\":{\"used_percent\":42}}}\n\n",
                "event: codex.rate_limits\ndata: not-json-data\n\n",
                "event: codex.response.metadata\n\n",
                CREATED,
                COMPLETED,
            ],
        );
        assert_eq!(out, format!("{CREATED}{COMPLETED}"));
        assert_eq!(framer.data_frames, 2);
    }

    // TestResponsesSSEFramer_CodexClientFiltersRateLimitsAndTiming
    #[test]
    fn codex_client_filters_rate_limits_and_timing() {
        let mut framer = Framer::new(true);
        let out = write(
            &mut framer,
            &[RATE_LIMITS, METADATA, TIMING, CREATED, COMPLETED],
        );
        assert_eq!(out, format!("{METADATA}{CREATED}{COMPLETED}"));
    }

    #[test]
    fn passes_frames_it_cannot_read() {
        let mut framer = Framer::new(false);
        let out = write(
            &mut framer,
            &["event: ping\n\n", ": comment\n\n", "data: [DONE]\n\n"],
        );
        assert_eq!(out, "event: ping\n\n: comment\n\ndata: [DONE]\n\n");
        assert_eq!(framer.data_frames, 1);
        assert_eq!(framer.last_event, "");
    }

    #[test]
    fn ends_events_as_they_came() {
        let mut out = BytesMut::new();
        write_sse_chunk(&mut out, b"a\r\n");
        write_sse_chunk(&mut out, b"b\n");
        write_sse_chunk(&mut out, b"c");
        write_sse_chunk(&mut out, b"d\r\n\r\n");
        write_sse_chunk(&mut out, b"");
        assert_eq!(&out[..], b"a\r\n\r\nb\n\nc\n\nd\r\n\r\n");
        let frame_len = |chunk: &[u8]| frame_end(chunk, 0, &mut Work::default());
        assert_eq!(frame_len(b"a\r\n\r\nb\n\n"), Some(5));
        assert_eq!(frame_len(b"a\n\nb\r\n\r\n"), Some(3));
        assert_eq!(frame_len(b"a\n"), None);
    }

    /// Upstream's `WriteChunk` and `Flush`, as this read them before it read
    /// on from where it left off, to check against.
    mod upstream {
        use crate::sse_check::data_lines_valid;

        use super::super::{BytesMut, Framer, go, needs_line_break};

        pub(super) struct Upstream {
            pub(super) framer: Framer,
            pending: Vec<u8>,
        }

        impl Upstream {
            pub(super) fn new(is_codex_client: bool) -> Self {
                Self {
                    framer: Framer::new(is_codex_client),
                    pending: Vec::new(),
                }
            }

            pub(super) fn write_chunk(&mut self, out: &mut BytesMut, chunk: &[u8]) {
                let framer = &mut self.framer;
                if chunk.is_empty() || !framer.terminal_event.is_empty() {
                    return;
                }
                if starts_new_data_frame(&self.pending, chunk) {
                    let frame = std::mem::take(&mut self.pending);
                    framer.write_frame(out, &frame);
                    if !framer.terminal_event.is_empty() {
                        return;
                    }
                }
                if needs_line_break(&self.pending, chunk) {
                    self.pending.push(b'\n');
                }
                self.pending.extend_from_slice(chunk);
                loop {
                    let len = frame_len(&self.pending);
                    if len == 0 {
                        break;
                    }
                    let frame: Vec<u8> = self.pending.drain(..len).collect();
                    framer.write_frame(out, &frame);
                    if !framer.terminal_event.is_empty() {
                        self.pending.clear();
                        return;
                    }
                }
                if go::trim_space(&self.pending).is_empty() {
                    self.pending.clear();
                    return;
                }
                if self.pending.is_empty() || !can_emit_without_delimiter(&self.pending) {
                    return;
                }
                let frame = std::mem::take(&mut self.pending);
                framer.write_frame(out, &frame);
            }

            pub(super) fn flush(&mut self, out: &mut BytesMut) {
                if self.pending.is_empty() || !self.framer.terminal_event.is_empty() {
                    return;
                }
                if go::trim_space(&self.pending).is_empty()
                    || !can_flush_without_delimiter(&self.pending)
                {
                    self.pending.clear();
                    return;
                }
                let frame = std::mem::take(&mut self.pending);
                self.framer.write_frame(out, &frame);
            }
        }

        fn frame_len(chunk: &[u8]) -> usize {
            let find = |needle: &[u8]| chunk.windows(needle.len()).position(|w| w == needle);
            match (find(b"\n\n"), find(b"\r\n\r\n")) {
                (None, None) => 0,
                (None, Some(crlf)) => crlf + 4,
                (Some(lf), None) => lf + 2,
                (Some(lf), Some(crlf)) if lf < crlf => lf + 2,
                (Some(_), Some(crlf)) => crlf + 4,
            }
        }

        fn needs_more_data(chunk: &[u8]) -> bool {
            let trimmed = go::trim_space(chunk);
            !trimmed.is_empty() && has_field(trimmed, b"event:") && !has_field(trimmed, b"data:")
        }

        fn has_field(chunk: &[u8], prefix: &[u8]) -> bool {
            chunk
                .split(|&b| b == b'\n')
                .any(|line| go::trim_space(line).starts_with(prefix))
        }

        fn can_emit_without_delimiter(chunk: &[u8]) -> bool {
            let trimmed = go::trim_space(chunk);
            if trimmed.is_empty()
                || needs_more_data(trimmed)
                || !has_field(trimmed, b"event:")
                || !has_field(trimmed, b"data:")
            {
                return false;
            }
            data_lines_valid(trimmed)
        }

        fn can_flush_without_delimiter(chunk: &[u8]) -> bool {
            let trimmed = go::trim_space(chunk);
            !trimmed.is_empty() && has_field(trimmed, b"data:") && data_lines_valid(trimmed)
        }

        fn starts_new_data_frame(pending: &[u8], chunk: &[u8]) -> bool {
            let pending = go::trim_space(pending);
            if pending.is_empty()
                || has_field(pending, b"event:")
                || !has_field(pending, b"data:")
                || !data_lines_valid(pending)
            {
                return false;
            }
            let start = chunk
                .iter()
                .position(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                .unwrap_or(chunk.len());
            chunk[start..].starts_with(b"data:")
        }
    }

    /// A small, seeded generator, so a failure can be run again.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    // Against upstream's reading, which reads the held-back event again for
    // each chunk, over streams cut at random.
    #[test]
    fn frames_as_upstream_does() {
        let tokens: &[&[u8]] = &[
            b"event: response.output_text.delta",
            b"event: codex.rate_limits",
            b"event: ping",
            b"event:",
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}",
            b"data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}",
            b"data: {\"type\":\"codex.rate_limits\"}",
            b"data: {\"a\":",
            b"data: 1}",
            b"data: [DONE]",
            b"data:",
            b"da",
            b"ta: {}",
            b"{",
            b"}",
            b"x",
            b": c",
            b"id: 1",
            b"\n",
            b"\n",
            b"\n",
            b"\n\n",
            b"\r\n",
            b"\r\n\r\n",
            b"\r",
            b" ",
            b"\t",
            b"\xc2\xa0",
            b"\xe3\x80\x80",
            b"\xe3\x80",
            b"\xff",
        ];
        let ends: &[&[u8]] = &[
            b"event: response.completed",
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[]}}",
            b"data: {\"type\":\"error\",\"error\":{\"message\":\"m\"}}",
        ];
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..10_000 {
            let mut stream = Vec::new();
            for _ in 0..rng.below(40) {
                let token = if rng.below(40) == 0 {
                    ends[rng.below(ends.len())]
                } else {
                    tokens[rng.below(tokens.len())]
                };
                stream.extend_from_slice(token);
            }
            let codex = rng.below(2) == 0;
            let mut upstream = upstream::Upstream::new(codex);
            let mut framer = Framer::new(codex);
            let (mut want, mut got) = (BytesMut::new(), BytesMut::new());
            let mut at = 0;
            while at < stream.len() {
                let end = (at + 1 + rng.below(12)).min(stream.len());
                upstream.write_chunk(&mut want, &stream[at..end]);
                framer.write_chunk(&mut got, &stream[at..end]).unwrap();
                assert_eq!(got, want, "{:?} cut at {end}", go::quote_bytes(&stream));
                at = end;
            }
            upstream.flush(&mut want);
            framer.flush(&mut got);
            assert_eq!(got, want, "{:?}", go::quote_bytes(&stream));
            let upstream = upstream.framer;
            assert_eq!(
                (framer.terminal_event, framer.last_event, framer.data_frames),
                (
                    upstream.terminal_event,
                    upstream.last_event,
                    upstream.data_frames
                ),
            );
        }
    }

    /// A delta event whose data is whole only with its last three bytes, `}`
    /// and a blank line, with `len` bytes of text.
    fn delta_of(len: usize) -> Vec<u8> {
        let mut event = b"event: response.output_text.delta\n\
            data: {\"type\":\"response.output_text.delta\",\"delta\":\""
            .to_vec();
        event.resize(event.len() + len, b'x');
        event.extend_from_slice(b"\"}\n\n");
        event
    }

    #[test]
    fn stops_an_event_past_the_limit() {
        let mut framer = Framer::new(false).with_limit(64);
        let mut out = BytesMut::new();
        framer.write_chunk(&mut out, b"data: {}").unwrap();
        framer.write_chunk(&mut out, &[b'x'; 56]).unwrap();
        assert_eq!(framer.held(), 64);
        let error = framer.write_chunk(&mut out, b"x").unwrap_err();
        assert_eq!(error.status, 502);
        assert_eq!(error.text, "upstream SSE event exceeds 64 bytes");
        assert_eq!(framer.held(), 0);
        framer.flush(&mut out);
        assert!(out.is_empty());

        // Events before it in the same chunk are written first.
        let mut framer = Framer::new(false).with_limit(64);
        let mut chunk = CREATED.as_bytes().to_vec();
        chunk.extend_from_slice(&delta_of(100)[..100]);
        let error = framer.write_chunk(&mut out, &chunk).unwrap_err();
        assert_eq!(error.text, "upstream SSE event exceeds 64 bytes");
        assert_eq!(&out[..], CREATED.as_bytes());
        assert_eq!(framer.data_frames, 1);
        assert_eq!(framer.held(), 0);
    }

    #[test]
    fn passes_an_event_at_the_limit() {
        let event = delta_of(1000);
        // Held back, the event is at most all but its last three bytes.
        let (held, rest) = event.split_at(event.len() - 3);
        let mut framer = Framer::new(false).with_limit(held.len());
        let mut out = BytesMut::new();
        framer.write_chunk(&mut out, held).unwrap();
        assert!(out.is_empty());
        framer.write_chunk(&mut out, rest).unwrap();
        assert_eq!(&out[..], &event[..]);

        let mut framer = Framer::new(false).with_limit(held.len() - 1);
        let error = framer.write_chunk(&mut out, held).unwrap_err();
        assert_eq!(
            error.text,
            format!("upstream SSE event exceeds {} bytes", held.len() - 1)
        );
    }

    // Holding back a large event costs time in step with its size, and
    // holds back no more than it. Each byte is read about twice: by the
    // search for a blank line, and by the scan of the event. Reading the
    // held-back event again for each chunk would pass the bound within a few
    // chunks, so the bound is checked after each one.
    #[test]
    fn holds_back_a_large_event_in_linear_time() {
        let event = delta_of(6 << 20);
        let mut framer = Framer::new(false);
        let mut out = BytesMut::new();
        let mut fed = 0;
        for chunk in event.chunks(1024) {
            framer.write_chunk(&mut out, chunk).unwrap();
            fed += chunk.len();
            assert!(framer.work() <= 3 * fed, "{} after {fed}", framer.work());
            if out.is_empty() {
                assert_eq!(framer.held(), fed);
                assert!(framer.pending.capacity() <= 2 * fed + 1024);
            }
        }
        assert_eq!(&out[..], &event[..]);
        assert_eq!(framer.held(), 0);

        // An event too large fails as soon as it is, still in linear time.
        let mut framer = Framer::new(false).with_limit(1 << 20);
        let mut out = BytesMut::new();
        let mut failed = None;
        let mut fed = 0;
        for (i, chunk) in event.chunks(1024).enumerate() {
            let result = framer.write_chunk(&mut out, chunk);
            fed += chunk.len();
            assert!(framer.work() <= 3 * fed, "{} after {fed}", framer.work());
            if result.is_err() {
                failed = Some(i);
                break;
            }
            assert!(framer.held() <= 1 << 20);
        }
        assert_eq!(failed, Some(1024));
        assert!(out.is_empty());
    }

    #[test]
    fn stops_at_the_first_terminal_event() {
        let mut framer = Framer::new(false);
        let out = write(&mut framer, &[&format!("{COMPLETED}{CREATED}"), CREATED]);
        assert_eq!(out, COMPLETED);
        assert_eq!(framer.last_event, "response.completed");
    }

    #[test]
    fn reads_the_status_of_an_error_payload() {
        let mut framer = Framer::new(false);
        write(
            &mut framer,
            &[
                "data: {\"status\":200,\"error\":{\"status_code\":\"429\"},\"sequence_number\":\"4\"}\n\n",
            ],
        );
        let error = framer.terminal_error.unwrap();
        assert_eq!(error.status, 429);
        assert_eq!(
            error.text,
            "{\"error\":{\"status_code\":\"429\"},\"sequence_number\":\"4\"}"
        );
        assert_eq!(
            payload_error_message(b"{\"code\":1,\"message\":2}").status,
            502
        );
    }
}
