// Literal, rescanning port of CLIProxyAPI v8.0.47 (90c447d3), MIT:
// sdk/api/handlers/openai/openai_responses_handlers.go:83-249,427-750.
// https://github.com/router-for-me/CLIProxyAPI
//
// This is a framing oracle, not a second implementation of error sanitization.
// Its sink records recovered frames before repairFrame. No production SSE helper,
// scanner, prefix buffer, recovery function or emit predicate is used here.
// Go-compatible JSON and whitespace primitives are shared, not framing logic.

use bytes::BytesMut;
use open_ferry_translate::go;

use crate::json;

#[derive(Default)]
pub(super) struct Upstream {
    pending: Vec<u8>,
    next_frame_prefix: Vec<u8>,
    next_field_suffix: Vec<u8>,
    synthetic_breaks: Vec<usize>,
    pub(super) frames: Vec<Vec<u8>>,
    pub(super) terminal_event: String,
    is_codex_client: bool,
}

impl Upstream {
    pub(super) fn new(is_codex_client: bool) -> Self {
        Self {
            is_codex_client,
            ..Self::default()
        }
    }

    pub(super) fn write_chunk(&mut self, chunk: &[u8]) {
        if chunk.is_empty() || !self.terminal_event.is_empty() {
            return;
        }
        let mut chunk = chunk.to_vec();
        if !self.next_field_suffix.is_empty() {
            let (_, complete, _) = chunk_field(trim_left(&chunk, b" \t\r\n"));
            let last = last_line(&self.next_frame_prefix);
            if complete && !last.is_empty() && !last.starts_with(b"data:") {
                self.next_frame_prefix.append(&mut self.next_field_suffix);
            } else {
                self.next_field_suffix.extend_from_slice(&chunk);
                let (_, _, partial) = chunk_field(trim_left(&self.next_field_suffix, b" \t\r\n"));
                if partial {
                    return;
                }
                chunk = std::mem::take(&mut self.next_field_suffix);
            }
        }
        if !self.next_frame_prefix.is_empty() {
            if let Some(payload) = data_payload(&self.next_frame_prefix)
                && go::json_valid(&payload)
                && json::str_at(&payload, "type").is_empty()
                && !has_field(&self.next_frame_prefix, b"event:")
                && frame_len(&self.next_frame_prefix) == 0
            {
                let trimmed = trim_left(&chunk, b" \t\r\n");
                let (field, mut complete, partial) = chunk_field(trimmed);
                let last = last_line(&self.next_frame_prefix);
                let (_, _, last_partial) = chunk_field(last);
                if complete && last_partial {
                    let joined = [last, trimmed].concat();
                    let (joined_field, joined_complete, _) = chunk_field(&joined);
                    if joined_complete
                        && (matches!(joined_field, b"event" | b"data" | b"id" | b"retry")
                            || (!field.is_empty()
                                && !matches!(field, b"event" | b"data" | b"id" | b"retry")))
                    {
                        complete = false;
                    }
                }
                let mut new_field_prefix = last.starts_with(b"data:");
                for prefix in [b"event:".as_slice(), b"data:", b"id:", b"retry:"] {
                    new_field_prefix |= trimmed.len() < prefix.len() && prefix.starts_with(trimmed);
                }
                if partial && new_field_prefix {
                    self.next_field_suffix.extend_from_slice(&chunk);
                    return;
                }
                if complete
                    && !self.next_frame_prefix.ends_with(b"\n")
                    && !self.next_frame_prefix.ends_with(b"\r")
                    && chunk[0] != b'\n'
                    && chunk[0] != b'\r'
                {
                    self.next_frame_prefix.push(b'\n');
                }
            }
            self.next_frame_prefix.extend_from_slice(&chunk);
            chunk = std::mem::take(&mut self.next_frame_prefix);
        }
        if has_empty_event_data(&self.pending) {
            let trimmed = trim_left(&chunk, b" \t\r\n");
            let (field, complete, partial) = chunk_field(trimmed);
            if partial {
                self.next_frame_prefix.extend_from_slice(&chunk);
                return;
            }
            let mut new_frame = complete && field != b"data";
            if complete && field == b"data" {
                let payload = data_payload(trimmed).unwrap_or_default();
                let valid = go::json_valid(&payload);
                if !valid && payload != b"[DONE]" && frame_len(trimmed) == 0 {
                    self.next_frame_prefix.extend_from_slice(&chunk);
                    return;
                }
                let has_event = has_field(trimmed, b"event:");
                if valid && has_event && event_name(trimmed).is_empty() && frame_len(trimmed) == 0 {
                    self.next_frame_prefix.extend_from_slice(&chunk);
                    return;
                }
                new_frame = payload == b"[DONE]"
                    || (valid && (!json::str_at(&payload, "type").is_empty() || has_event));
                if valid && !new_frame && frame_len(trimmed) == 0 {
                    self.next_frame_prefix.extend_from_slice(&chunk);
                    return;
                }
            }
            if new_frame {
                self.write_pending();
                if !self.terminal_event.is_empty() {
                    return;
                }
            }
        }
        if starts_new_data_frame(&self.pending, &chunk) {
            self.write_pending();
            if !self.terminal_event.is_empty() {
                return;
            }
        }
        append_chunk(&mut self.pending, &mut self.synthetic_breaks, &chunk);
        loop {
            let len = frame_len(&self.pending);
            if len == 0 {
                break;
            }
            let frame = self.pending[..len].to_vec();
            self.write_frame(&frame);
            self.pending.drain(..len);
            self.synthetic_breaks = self
                .synthetic_breaks
                .iter()
                .filter_map(|&pos| pos.checked_sub(len))
                .collect();
            if !self.terminal_event.is_empty() {
                self.pending.clear();
                self.synthetic_breaks.clear();
                return;
            }
        }
        if go::trim_space(&self.pending).is_empty() {
            self.pending.clear();
            self.synthetic_breaks.clear();
            return;
        }
        if self.pending.is_empty()
            || has_empty_event_data(&self.pending)
            || !can_emit_without_delimiter(&self.pending)
        {
            return;
        }
        self.write_pending();
    }

    pub(super) fn flush(&mut self) {
        if !self.next_field_suffix.is_empty() {
            if needs_line_break(&self.next_frame_prefix, &self.next_field_suffix) {
                self.next_frame_prefix.push(b'\n');
            }
            self.next_frame_prefix.append(&mut self.next_field_suffix);
        }
        if !self.next_frame_prefix.is_empty() {
            let trimmed = trim_left(&self.next_frame_prefix, b" \t\r\n");
            let (_, _, partial) = chunk_field(trimmed);
            if partial && !b"data".starts_with(trimmed) {
                self.write_pending();
            }
            append_chunk(
                &mut self.pending,
                &mut self.synthetic_breaks,
                &self.next_frame_prefix,
            );
            self.next_frame_prefix.clear();
        }
        if self.pending.is_empty() || !self.terminal_event.is_empty() {
            return;
        }
        if go::trim_space(&self.pending).is_empty() {
            self.pending.clear();
            return;
        }
        let frame = recover_json_frame(&self.pending, &self.synthetic_breaks);
        if !can_flush_without_delimiter(&frame) {
            self.pending.clear();
            return;
        }
        self.write_frame(&frame);
        self.pending.clear();
        self.synthetic_breaks.clear();
    }

    fn write_pending(&mut self) {
        let frame = self.pending.clone();
        self.write_frame(&frame);
        self.pending.clear();
        self.synthetic_breaks.clear();
    }

    fn write_frame(&mut self, frame: &[u8]) {
        let frame = recover_json_frame(frame, &self.synthetic_breaks);
        self.observe_terminal(&frame);
        self.frames.push(frame);
    }

    // Only repairFrame's terminal/filter decisions affect WriteChunk/Flush.
    // Record its input instead of duplicating unrelated error/output rewriting.
    fn observe_terminal(&mut self, frame: &[u8]) {
        let event = event_name(frame);
        if private(&event, self.is_codex_client) {
            return;
        }
        let Some(payload) = data_payload(frame).filter(|data| !data.is_empty()) else {
            return;
        };
        if payload == b"[DONE]" {
            return;
        }
        let kind = json::str_at(&payload, "type");
        if private(&kind, self.is_codex_client) || !go::json_valid(&payload) {
            return;
        }
        let has_error = json::get(&payload, "error").is_some_and(|error| error.raw != b"null")
            || json::get(&payload, "response.error").is_some_and(|error| error.raw != b"null")
            || (json::get(&payload, "code").is_some() && json::get(&payload, "message").is_some());
        let event_type = if terminal(&event) || kind.is_empty() {
            &event
        } else {
            &kind
        };
        if error_event(&kind) || has_error || error_event(event_type) {
            self.terminal_event = if kind == "response.failed" || self.is_codex_client {
                "response.failed"
            } else {
                "error"
            }
            .to_owned();
        } else if terminal(event_type) {
            self.terminal_event = event_type.to_owned();
        }
    }
}

fn error_event(name: &str) -> bool {
    matches!(name, "error" | "response.failed" | "response.error")
}

fn terminal(name: &str) -> bool {
    matches!(
        name,
        "response.completed"
            | "response.incomplete"
            | "response.failed"
            | "response.done"
            | "response.error"
            | "error"
    )
}

fn private(name: &str, codex: bool) -> bool {
    let name = name.trim();
    if name.is_empty() || error_event(name) {
        return false;
    }
    name.starts_with("responsesapi.")
        || name == "response.subscription_usage"
        || if codex {
            name == "codex.rate_limits"
        } else {
            name.starts_with("codex.") || name == "response.metadata"
        }
}

fn trim_left<'a>(bytes: &'a [u8], chars: &[u8]) -> &'a [u8] {
    &bytes[bytes
        .iter()
        .position(|b| !chars.contains(b))
        .unwrap_or(bytes.len())..]
}

fn last_line(bytes: &[u8]) -> &[u8] {
    go::trim_space(bytes.rsplit(|&b| b == b'\n').next().unwrap_or_default())
}

fn chunk_field(chunk: &[u8]) -> (&[u8], bool, bool) {
    for (i, &byte) in chunk.iter().enumerate() {
        if matches!(byte, b':' | b'\n' | b'\r') {
            return (&chunk[..i], true, false);
        }
        if !byte.is_ascii_alphanumeric() && !matches!(byte, b'-' | b'_' | b'.') {
            return (b"", false, false);
        }
    }
    (b"", false, !chunk.is_empty())
}

fn data_payload(frame: &[u8]) -> Option<Vec<u8>> {
    let mut payload = Vec::new();
    let mut found = false;
    for line in frame.split(|&b| b == b'\n') {
        let end = line
            .iter()
            .rposition(|&byte| byte != b'\r')
            .map_or(0, |i| i + 1);
        let line = &line[..end];
        let trimmed = go::trim_space(line);
        if let Some(data) = trimmed.strip_prefix(b"data:") {
            if found {
                payload.push(b'\n');
            }
            payload.extend_from_slice(go::trim_space(data));
            found = true;
        }
    }
    found.then_some(payload)
}

fn frame_with_data(frame: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in frame.split(|&b| b == b'\n') {
        let end = line.iter().rposition(|&b| b != b'\r').map_or(0, |i| i + 1);
        let line = &line[..end];
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

fn recover_json_frame(frame: &[u8], breaks: &[usize]) -> Vec<u8> {
    if breaks.is_empty() || data_payload(frame).is_some_and(|payload| go::json_valid(&payload)) {
        return frame.to_vec();
    }
    let positions: Vec<_> = breaks
        .iter()
        .copied()
        .filter(|&pos| frame.get(pos) == Some(&b'\n'))
        .collect();
    let recover = |candidate: &[u8]| {
        let payload = data_payload(candidate)?;
        go::json_valid(&payload).then(|| frame_with_data(candidate, &payload))
    };
    for &pos in positions.iter().take(8) {
        let candidate = [&frame[..pos], &frame[pos + 1..]].concat();
        if let Some(frame) = recover(&candidate) {
            return frame;
        }
    }
    if positions.len() > 1 {
        let mut candidate = Vec::new();
        let mut start = 0;
        for pos in positions {
            candidate.extend_from_slice(&frame[start..pos]);
            start = pos + 1;
        }
        candidate.extend_from_slice(&frame[start..]);
        if let Some(frame) = recover(&candidate) {
            return frame;
        }
    }
    frame.to_vec()
}

fn append_chunk(pending: &mut Vec<u8>, breaks: &mut Vec<usize>, chunk: &[u8]) {
    if needs_line_break(pending, chunk) {
        breaks.push(pending.len());
        pending.push(b'\n');
    }
    pending.extend_from_slice(chunk);
}

fn needs_line_break(pending: &[u8], chunk: &[u8]) -> bool {
    if pending.is_empty()
        || chunk.is_empty()
        || pending.ends_with(b"\n")
        || pending.ends_with(b"\r")
        || chunk[0] == b'\n'
        || chunk[0] == b'\r'
    {
        return false;
    }
    let trimmed = trim_left(chunk, b" \t");
    for prefix in [b"data:".as_slice(), b"event:", b"id:", b"retry:", b":"] {
        if trimmed.starts_with(prefix) {
            return !data_ends_inside_json_string(pending);
        }
    }
    false
}

fn data_ends_inside_json_string(frame: &[u8]) -> bool {
    let Some(payload) = data_payload(frame) else {
        return false;
    };
    let payload = go::trim_space(&payload);
    if !matches!(payload.first(), Some(b'{' | b'[' | b'"')) {
        return false;
    }
    let (mut in_string, mut escaped) = (false, false);
    for &byte in payload {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string && byte == b'\\' {
            escaped = true;
            continue;
        }
        if byte == b'"' {
            in_string = !in_string;
        }
    }
    in_string
}

fn frame_len(chunk: &[u8]) -> usize {
    let lf = chunk.windows(2).position(|w| w == b"\n\n");
    let crlf = chunk.windows(4).position(|w| w == b"\r\n\r\n");
    match (lf, crlf) {
        (None, None) => 0,
        (None, Some(crlf)) => crlf + 4,
        (Some(lf), None) => lf + 2,
        (Some(lf), Some(crlf)) if lf < crlf => lf + 2,
        (Some(_), Some(crlf)) => crlf + 4,
    }
}

fn has_field(chunk: &[u8], prefix: &[u8]) -> bool {
    chunk
        .split(|&b| b == b'\n')
        .any(|line| go::trim_space(line).starts_with(prefix))
}

fn event_name(frame: &[u8]) -> String {
    frame
        .split(|&b| b == b'\n')
        .find_map(|line| go::trim_space(line).strip_prefix(b"event:"))
        .map(|name| String::from_utf8_lossy(name).trim().to_owned())
        .unwrap_or_default()
}

fn data_lines_valid(chunk: &[u8]) -> bool {
    let Some(payload) = data_payload(chunk) else {
        return true;
    };
    let payload = go::trim_space(&payload);
    payload.is_empty() || payload == b"[DONE]" || go::json_valid(payload)
}

fn needs_more_data(chunk: &[u8]) -> bool {
    let trimmed = go::trim_space(chunk);
    !trimmed.is_empty() && has_field(trimmed, b"event:") && !has_field(trimmed, b"data:")
}

fn has_empty_event_data(chunk: &[u8]) -> bool {
    has_field(chunk, b"event:")
        && data_payload(chunk).is_some_and(|data| go::trim_space(&data).is_empty())
}

fn can_emit_without_delimiter(chunk: &[u8]) -> bool {
    let trimmed = go::trim_space(chunk);
    !trimmed.is_empty()
        && !needs_more_data(trimmed)
        && has_field(trimmed, b"event:")
        && has_field(trimmed, b"data:")
        && data_lines_valid(trimmed)
}

fn can_flush_without_delimiter(chunk: &[u8]) -> bool {
    let trimmed = go::trim_space(chunk);
    !trimmed.is_empty() && has_field(trimmed, b"data:") && data_lines_valid(trimmed)
}

fn starts_new_data_frame(pending: &[u8], chunk: &[u8]) -> bool {
    let pending = go::trim_space(pending);
    !pending.is_empty()
        && !has_field(pending, b"event:")
        && has_field(pending, b"data:")
        && data_lines_valid(pending)
        && trim_left(chunk, b" \t\r\n").starts_with(b"data:")
}

pub(super) fn wire(frames: &[Vec<u8>]) -> BytesMut {
    let mut out = BytesMut::new();
    for frame in frames {
        if frame.is_empty() {
            continue;
        }
        out.extend_from_slice(frame);
        if !frame.ends_with(b"\n\n") && !frame.ends_with(b"\r\n\r\n") {
            out.extend_from_slice(if frame.ends_with(b"\r\n") {
                b"\r\n"
            } else if frame.ends_with(b"\n") {
                b"\n"
            } else {
                b"\n\n"
            });
        }
    }
    out
}
