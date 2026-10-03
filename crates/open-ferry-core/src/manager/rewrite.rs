// Ported from CLIProxyAPI sdk/cliproxy/auth/response_model_rewriter.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Naming the alias in responses. When an OAuth alias is force-mapped, the
//! client asked for the alias and should see it back, so the upstream
//! model's name is replaced in each response's model fields, in whole JSON
//! bodies and in SSE `data:` lines split across chunks.
//!
//! Deviations from upstream:
//! - A JSON document with a model field is re-serialized through
//!   `serde_json` (compact, key order kept), where upstream edits the field
//!   in place; whitespace around the document is kept.
//! - The debug logging of rewritten paths isn't ported.

use serde_json::Value;

/// The fields a model name is written to (upstream's `modelFieldPaths`).
const MODEL_FIELD_PATHS: [&[&str]; 5] = [
    &["model"],
    &["modelVersion"],
    &["response", "model"],
    &["response", "modelVersion"],
    &["message", "model"],
];

/// The most a chunk may hold, with what was held back, before it passes
/// through untouched (upstream's `maxPendingBufSize`).
const MAX_PENDING_BUF_SIZE: usize = 1 << 20;

fn is_json_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Whether `data` is one valid JSON value (gjson's `ValidBytes`).
fn valid_json(data: &[u8]) -> bool {
    !data.is_empty() && serde_json::from_slice::<Value>(data).is_ok()
}

fn trim_space(data: &[u8]) -> &[u8] {
    let start = data
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(data.len());
    let end = data
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &data[start..end]
}

fn path_slot<'v>(value: &'v mut Value, path: &[&str]) -> Option<&'v mut Value> {
    let mut slot = value;
    for key in path {
        slot = slot.as_object_mut()?.get_mut(*key)?;
    }
    Some(slot)
}

/// Sets every model field present in `data` to `target` (upstream's
/// `rewriteModelInResponse`). Returns `data` unchanged when there is none
/// or it isn't JSON.
pub(crate) fn rewrite_model_in_response(data: &[u8], target: &str) -> Vec<u8> {
    if target.is_empty() || data.is_empty() {
        return data.to_vec();
    }
    let start = data
        .iter()
        .position(|b| !is_json_space(*b))
        .unwrap_or(data.len());
    let end = data
        .iter()
        .rposition(|b| !is_json_space(*b))
        .map_or(start, |i| i + 1);
    let Ok(mut value) = serde_json::from_slice::<Value>(&data[start..end]) else {
        return data.to_vec();
    };
    let mut rewrote = false;
    for path in MODEL_FIELD_PATHS {
        if let Some(slot) = path_slot(&mut value, path) {
            *slot = Value::String(target.to_owned());
            rewrote = true;
        }
    }
    if !rewrote {
        return data.to_vec();
    }
    let Ok(body) = serde_json::to_vec(&value) else {
        return data.to_vec();
    };
    let mut out = Vec::with_capacity(body.len() + data.len() - (end - start));
    out.extend_from_slice(&data[..start]);
    out.extend_from_slice(&body);
    out.extend_from_slice(&data[end..]);
    out
}

/// A `data:` line's prefix and payload (upstream's `extractSSEDataLine`).
pub(super) fn extract_sse_data_line(line: &[u8]) -> Option<(&'static [u8], &[u8])> {
    if let Some(rest) = line.strip_prefix(b"data: ") {
        return Some((b"data: ", rest));
    }
    line.strip_prefix(b"data:")
        .map(|rest| (&b"data:"[..], rest))
}

fn split_lines(payload: &[u8]) -> Vec<&[u8]> {
    payload.split(|b| *b == b'\n').collect()
}

fn join_lines(lines: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(line);
    }
    out
}

/// Rewrites the model in every JSON `data:` line (upstream's
/// `rewriteSSEPayloadLines`).
pub(crate) fn rewrite_sse_payload_lines(payload: &[u8], target: &str) -> Vec<u8> {
    if target.is_empty() || payload.is_empty() {
        return payload.to_vec();
    }
    let out: Vec<Vec<u8>> = split_lines(payload)
        .into_iter()
        .map(|line| match extract_sse_data_line(line) {
            Some((prefix, json)) if json.first() == Some(&b'{') && valid_json(json) => {
                let mut rewritten = prefix.to_vec();
                rewritten.extend_from_slice(&rewrite_model_in_response(json, target));
                rewritten
            }
            _ => line.to_vec(),
        })
        .collect();
    let mut joined = join_lines(&out);
    if payload.last() == Some(&b'\n') && joined.last() != Some(&b'\n') {
        joined.push(b'\n');
    }
    joined
}

/// The payload of the last `data:` line with one (upstream's
/// `extractLastDataPayload`).
fn extract_last_data_payload(chunk: &[u8]) -> &[u8] {
    for line in split_lines(chunk).into_iter().rev() {
        if let Some((_, json)) = extract_sse_data_line(line)
            && !json.is_empty()
        {
            return json;
        }
    }
    &[]
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

/// Splits SSE events glued without their separator, but only where the
/// bytes before close a valid JSON `data:` line (upstream's
/// `safeReplaceGlued`).
fn safe_replace_glued(chunk: Vec<u8>, old: &[u8], new: &[u8]) -> Vec<u8> {
    if old.is_empty() || chunk.is_empty() || find(&chunk, old).is_none() {
        return chunk;
    }
    let mut result = Vec::with_capacity(chunk.len() + 8);
    let mut remaining: &[u8] = &chunk;
    loop {
        let Some(idx) = find(remaining, old) else {
            result.extend_from_slice(remaining);
            break;
        };
        let part = match remaining[..idx].iter().rposition(|b| *b == b'\n') {
            Some(line_start) => &remaining[line_start + 1..=idx],
            None => &remaining[..=idx],
        };
        let glued = extract_sse_data_line(part).is_some_and(|(_, json)| valid_json(json));
        if glued {
            result.extend_from_slice(&remaining[..idx]);
            result.extend_from_slice(new);
        } else {
            result.extend_from_slice(&remaining[..idx + old.len()]);
        }
        remaining = &remaining[idx + old.len()..];
    }
    result
}

/// Upstream's `normalizeGluedSSEEvents`.
pub(super) fn normalize_glued_sse_events(chunk: Vec<u8>) -> Vec<u8> {
    if chunk.is_empty() {
        return chunk;
    }
    let chunk = safe_replace_glued(chunk, b"}event:", b"}\n\nevent:");
    let chunk = safe_replace_glued(chunk, b"}\r\nevent:", b"}\r\n\r\nevent:");
    let chunk = safe_replace_glued(chunk, b"}data:", b"}\ndata:");
    safe_replace_glued(chunk, b"}\r\ndata:", b"}\r\ndata:")
}

/// Rewrites the model in a stream of SSE chunks, holding back a partial
/// event until the rest arrives (upstream's `StreamRewriter`).
pub(crate) struct StreamRewriter {
    target: String,
    pending: Vec<u8>,
}

impl StreamRewriter {
    pub(crate) fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            pending: Vec::new(),
        }
    }

    fn rewrite_sse_lines(&self, payload: &[u8]) -> Vec<u8> {
        rewrite_sse_payload_lines(payload, &self.target)
    }

    /// Rewrites one chunk; empty when it is all held back (upstream's
    /// `RewriteChunk`).
    pub(crate) fn rewrite_chunk(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.target.is_empty() {
            return chunk.to_vec();
        }
        let mut chunk = chunk.to_vec();
        if !self.pending.is_empty() {
            let mut combined = std::mem::take(&mut self.pending);
            if combined.last() != Some(&b'\n') {
                combined.push(b'\n');
            }
            combined.extend_from_slice(&chunk);
            chunk = combined;
        }
        let chunk = normalize_glued_sse_events(chunk);
        if chunk.len() > MAX_PENDING_BUF_SIZE {
            return chunk;
        }
        let trimmed = trim_space(&chunk);
        if trimmed.first() == Some(&b'{') && valid_json(trimmed) {
            return rewrite_model_in_response(trimmed, &self.target);
        }

        let process: &[u8] = if let Some(last) = rfind(&chunk, b"\n\n") {
            let after = &chunk[last + 2..];
            if !after.is_empty() && after != b"\n" {
                self.pending = after.to_vec();
                &chunk[..last + 2]
            } else {
                &chunk
            }
        } else if valid_json(extract_last_data_payload(&chunk)) {
            &chunk
        } else if trim_space(&chunk).is_empty() {
            return chunk;
        } else {
            self.pending = chunk;
            return Vec::new();
        };

        let mut result: Vec<Vec<u8>> = Vec::new();
        let mut pending_event: Option<&[u8]> = None;
        for line in split_lines(process) {
            if line.starts_with(b"event:") {
                pending_event = Some(line);
                continue;
            }
            if let Some((prefix, json)) = extract_sse_data_line(line)
                && json.first() == Some(&b'{')
            {
                if !valid_json(json) {
                    match pending_event.take() {
                        Some(event) => {
                            let mut held = event.to_vec();
                            held.push(b'\n');
                            held.extend_from_slice(line);
                            self.pending = held;
                        }
                        None => self.pending.extend_from_slice(line),
                    }
                    continue;
                }
                if let Some(event) = pending_event.take() {
                    result.push(event.to_vec());
                }
                let mut rewritten = prefix.to_vec();
                rewritten.extend_from_slice(&rewrite_model_in_response(json, &self.target));
                result.push(rewritten);
                continue;
            }
            if let Some(event) = pending_event.take() {
                result.push(event.to_vec());
            }
            result.push(line.to_vec());
        }
        if let Some(event) = pending_event {
            result.push(event.to_vec());
        }
        let joined = join_lines(&result);
        if joined.is_empty() && !chunk.is_empty() {
            return self.rewrite_sse_lines(&chunk);
        }
        joined
    }

    /// Flushes whatever is held back at the end of the stream (upstream's
    /// `Finish`).
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(b"\n\n");
        let buf = normalize_glued_sse_events(buf);
        let mut out = self.rewrite_chunk(&buf);
        if !self.pending.is_empty() {
            let tail = self.rewrite_sse_lines(&self.pending);
            self.pending.clear();
            out.extend_from_slice(&tail);
        }
        out
    }

    /// Rewrites one stream chunk, falling back to a line-by-line rewrite of
    /// SSE text the rewriter held back; empty when nothing is ready yet
    /// (upstream's `rewriteForceMappedStreamChunk`).
    pub(crate) fn rewrite_stream_chunk(&mut self, payload: &[u8]) -> Vec<u8> {
        if payload.is_empty() {
            return Vec::new();
        }
        let rewritten = self.rewrite_chunk(payload);
        if !rewritten.is_empty() {
            return rewritten;
        }
        if find(payload, b"data:").is_some() {
            let line_wise = self.rewrite_sse_lines(payload);
            if !line_wise.is_empty() {
                return line_wise;
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn rewrites_every_model_field_present() {
        let out = rewrite_model_in_response(
            br#"{"model":"up","response":{"model":"up","x":1},"other":2}"#,
            "alias",
        );
        assert_eq!(
            s(&out),
            r#"{"model":"alias","response":{"model":"alias","x":1},"other":2}"#
        );
        let untouched = br#"{ "id": 1 }"#;
        assert_eq!(
            rewrite_model_in_response(untouched, "alias"),
            untouched.to_vec()
        );
    }

    #[test]
    fn rewrites_sse_lines_and_keeps_the_trailing_newline() {
        let out =
            rewrite_sse_payload_lines(b"event: x\ndata: {\"model\":\"up\"}\ndata: [DONE]\n", "a");
        assert_eq!(s(&out), "event: x\ndata: {\"model\":\"a\"}\ndata: [DONE]\n");
    }

    #[test]
    fn holds_an_unterminated_event_until_the_next_chunk() {
        let mut rewriter = StreamRewriter::new("alias");
        let first = rewriter.rewrite_chunk(b"data: {\"model\":\"up\"}\n\ndata: {\"model\":\"up\"}");
        assert_eq!(s(&first), "data: {\"model\":\"alias\"}\n\n");
        let second = rewriter.rewrite_chunk(b"\n\n");
        assert_eq!(s(&second), "data: {\"model\":\"alias\"}\n\n\n");
        assert!(rewriter.finish().is_empty());
    }

    #[test]
    fn holds_an_event_line_until_its_data_arrives() {
        let mut rewriter = StreamRewriter::new("alias");
        assert!(rewriter.rewrite_chunk(b"event: e\n").is_empty());
        let out = rewriter.rewrite_chunk(b"data: {\"model\":\"up\"}\n\n");
        assert_eq!(s(&out), "event: e\ndata: {\"model\":\"alias\"}\n\n");
    }

    #[test]
    fn splits_glued_events() {
        let mut rewriter = StreamRewriter::new("alias");
        let out = rewriter.rewrite_chunk(b"data: {\"model\":\"up\"}data: {\"model\":\"up\"}");
        assert_eq!(
            s(&out),
            "data: {\"model\":\"alias\"}\ndata: {\"model\":\"alias\"}"
        );
    }

    #[test]
    fn rewrites_raw_json_chunks() {
        let mut rewriter = StreamRewriter::new("alias");
        let out = rewriter.rewrite_chunk(b"  {\"modelVersion\":\"up\"}\n");
        assert_eq!(s(&out), "{\"modelVersion\":\"alias\"}");
    }

    #[test]
    fn finish_flushes_an_incomplete_tail() {
        let mut rewriter = StreamRewriter::new("alias");
        assert!(
            rewriter
                .rewrite_chunk(b"event: e\ndata: {\"model\"")
                .is_empty()
        );
        let out = rewriter.finish();
        assert_eq!(s(&out), "\nevent: e\ndata: {\"model\"");
    }
}
