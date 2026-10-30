// Ported from CLIProxyAPI responsesSSERecoverJSONFrame, responsesSSEChunkField,
// responsesSSEDataEndsInsideJSONString and responsesSSEFramer (v8.0.47, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;

const FIELDS: &[&[u8]] = &[b"event:", b"data:", b"id:", b"retry:"];

impl Framer {
    /// Disambiguates a new frame after an empty data field, retaining split
    /// field names until enough of them has arrived to make the decision.
    pub(super) fn prepare_chunk<'a>(
        &mut self,
        out: &mut BytesMut,
        chunk: &'a [u8],
    ) -> Option<Cow<'a, [u8]>> {
        let mut chunk = Cow::Borrowed(chunk);
        if !self.next_field_suffix.is_empty() {
            let (_, complete, _) = chunk_field(trim_start(&chunk));
            let last = last_line(&self.next_frame_prefix);
            if complete && !last.is_empty() && !last.starts_with(b"data:") {
                self.next_frame_prefix.append(&mut self.next_field_suffix);
            } else {
                self.next_field_suffix.extend_from_slice(&chunk);
                let (_, _, partial) = chunk_field(trim_start(&self.next_field_suffix));
                if partial {
                    return None;
                }
                chunk = Cow::Owned(std::mem::take(&mut self.next_field_suffix));
            }
        }
        if !self.next_frame_prefix.is_empty() {
            let payload = data_payload(&self.next_frame_prefix);
            if payload.as_ref().is_some_and(|payload| {
                go::json_valid(payload) && json::str_at(payload, "type").is_empty()
            }) && !has_field(&self.next_frame_prefix, b"event:")
                && frame_end(&self.next_frame_prefix, 0, &mut Work::default()).is_none()
            {
                let trimmed = trim_start(&chunk);
                let (field, mut complete, partial) = chunk_field(trimmed);
                let last = last_line(&self.next_frame_prefix);
                let (_, _, last_partial) = chunk_field(last);
                if complete && last_partial {
                    let joined = [last, trimmed].concat();
                    let (joined_field, joined_complete, _) = chunk_field(&joined);
                    if joined_complete
                        && (standard_field(joined_field)
                            || (!field.is_empty() && !standard_field(field)))
                    {
                        complete = false;
                    }
                }
                let new_field_prefix = last.starts_with(b"data:")
                    || FIELDS
                        .iter()
                        .any(|prefix| trimmed.len() < prefix.len() && prefix.starts_with(trimmed));
                if partial && new_field_prefix {
                    self.next_field_suffix.extend_from_slice(&chunk);
                    return None;
                }
                if complete && !ends_line(&self.next_frame_prefix) && !starts_line(&chunk) {
                    self.next_frame_prefix.push(b'\n');
                }
            }
            self.next_frame_prefix.extend_from_slice(&chunk);
            chunk = Cow::Owned(std::mem::take(&mut self.next_frame_prefix));
        }
        if has_empty_event_data(&self.pending, &self.scan) {
            let trimmed = trim_start(&chunk);
            let (field, complete, partial) = chunk_field(trimmed);
            if partial {
                self.next_frame_prefix.extend_from_slice(&chunk);
                return None;
            }
            // A bare word terminated by a real newline is data, not a new
            // colon-bearing field (upstream mistakes malformed string data here).
            let mut new_frame =
                complete && field != b"data" && trimmed.get(field.len()) == Some(&b':');
            if complete && field == b"data" {
                let payload = data_payload(trimmed).unwrap_or_default();
                let valid = go::json_valid(&payload);
                let delimited = frame_end(trimmed, 0, &mut Work::default()).is_some();
                if !valid && payload != b"[DONE]" && !delimited {
                    self.next_frame_prefix.extend_from_slice(&chunk);
                    return None;
                }
                let has_event = has_field(trimmed, b"event:");
                if valid && has_event && event_name(trimmed).is_empty() && !delimited {
                    self.next_frame_prefix.extend_from_slice(&chunk);
                    return None;
                }
                new_frame = payload == b"[DONE]"
                    || (valid && (!json::str_at(&payload, "type").is_empty() || has_event));
                if valid && !new_frame && !delimited {
                    self.next_frame_prefix.extend_from_slice(&chunk);
                    return None;
                }
            }
            if new_frame {
                let pending = self.take_pending();
                self.write_frame(out, &pending);
                if !self.terminal_event.is_empty() {
                    return None;
                }
            }
        }
        Some(chunk)
    }

    pub(super) fn flush_prefix(&mut self, out: &mut BytesMut) {
        if !self.next_field_suffix.is_empty() {
            if needs_line_break(&self.next_frame_prefix, &self.next_field_suffix) {
                self.next_frame_prefix.push(b'\n');
            }
            self.next_frame_prefix.append(&mut self.next_field_suffix);
        }
        if !self.next_frame_prefix.is_empty() {
            let trimmed = trim_start(&self.next_frame_prefix);
            let (_, _, partial) = chunk_field(trimmed);
            if partial && !b"data".starts_with(trimmed) {
                let pending = self.take_pending();
                self.write_frame(out, &pending);
            }
            if needs_line_break(&self.pending, &self.next_frame_prefix) {
                self.synthetic_breaks.push(self.pending.len());
                self.pending.push(b'\n');
            }
            self.pending.append(&mut self.next_frame_prefix);
            self.work.add(self.scan.advance(&self.pending));
        }
    }
}

fn trim_start(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn last_line(bytes: &[u8]) -> &[u8] {
    go::trim_space(bytes.rsplit(|&b| b == b'\n').next().unwrap_or_default())
}

fn standard_field(field: &[u8]) -> bool {
    matches!(field, b"event" | b"data" | b"id" | b"retry")
}

pub(super) fn chunk_field(chunk: &[u8]) -> (&[u8], bool, bool) {
    if chunk.is_empty() {
        return (b"", false, false);
    }
    for (i, &byte) in chunk.iter().enumerate() {
        if matches!(byte, b':' | b'\n' | b'\r') {
            return (&chunk[..i], true, false);
        }
        if !byte.is_ascii_alphanumeric() && !matches!(byte, b'-' | b'_' | b'.') {
            return (b"", false, false);
        }
    }
    (b"", false, true)
}

fn has_field(frame: &[u8], prefix: &[u8]) -> bool {
    frame
        .split(|&b| b == b'\n')
        .any(|line| go::trim_space(line).starts_with(prefix))
}

pub(super) fn has_empty_event_data(frame: &[u8], scan: &EventScan) -> bool {
    scan.has_event()
        && scan.has_data()
        && scan.data_valid()
        && data_payload(frame).is_some_and(|payload| go::trim_space(&payload).is_empty())
}

pub(super) fn consume_breaks(breaks: &mut Vec<usize>, consumed: usize) {
    breaks.retain_mut(|pos| {
        if *pos < consumed {
            false
        } else {
            *pos -= consumed;
            true
        }
    });
}

/// Only synthetic newlines are candidates for recovery. Real SSE comments,
/// newlines, escapes and string contents are never removed speculatively.
pub(super) fn recover_json_frame<'a>(frame: &'a [u8], breaks: &[usize]) -> Cow<'a, [u8]> {
    if breaks.is_empty() || data_payload(frame).is_some_and(|payload| go::json_valid(&payload)) {
        return Cow::Borrowed(frame);
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
            return Cow::Owned(frame);
        }
    }
    if !positions.is_empty() {
        let mut candidate = Vec::with_capacity(frame.len() - positions.len());
        let mut start = 0;
        for pos in positions {
            candidate.extend_from_slice(&frame[start..pos]);
            start = pos + 1;
        }
        candidate.extend_from_slice(&frame[start..]);
        if let Some(frame) = recover(&candidate) {
            return Cow::Owned(frame);
        }
        // Keep malformed private events classifiable across splits before JSON
        // colons. Only remove recorded synthetic newlines, never real comments.
        // Upstream recovers valid JSON only; its malformed type can be hidden
        // behind an inserted SSE comment and escape the public-client filter.
        if let Some(payload) = data_payload(&candidate) {
            let kind = json::str_at(&payload, "type");
            if kind == "response.metadata"
                || kind == "response.subscription_usage"
                || kind.starts_with("codex.")
                || kind.starts_with("responsesapi.")
            {
                return Cow::Owned(frame_with_data(&candidate, &payload));
            }
        }
    }
    Cow::Borrowed(frame)
}

fn ends_line(bytes: &[u8]) -> bool {
    bytes.ends_with(b"\n") || bytes.ends_with(b"\r")
}

fn starts_line(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\n") || bytes.starts_with(b"\r")
}

pub(super) fn needs_line_break(pending: &[u8], chunk: &[u8]) -> bool {
    if pending.is_empty() || chunk.is_empty() || ends_line(pending) || starts_line(chunk) {
        return false;
    }
    let trimmed = chunk.strip_prefix(b" ").unwrap_or(chunk);
    let start = trimmed
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t'))
        .unwrap_or(trimmed.len());
    let trimmed = &trimmed[start..];
    if !FIELDS
        .iter()
        .chain([&b":"[..]].iter())
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return false;
    }
    // Unlike upstream, do not split a fragmented standard SSE field name.
    // Otherwise `data` + `: {}` becomes `data\n: {}` and loses the payload.
    let last = last_line(pending);
    if FIELDS
        .iter()
        .any(|prefix| last.len() < prefix.len() && prefix.starts_with(last))
    {
        let joined = [last, trimmed].concat();
        if FIELDS.iter().any(|prefix| joined.starts_with(prefix)) {
            return false;
        }
    }
    !data_ends_inside_json_string(pending)
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
        } else if in_string && byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            in_string = !in_string;
        }
    }
    in_string
}
