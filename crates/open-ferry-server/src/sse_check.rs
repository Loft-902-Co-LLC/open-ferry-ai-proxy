// Ported from CLIProxyAPI sseJSONValidationState in
// sdk/api/handlers/handlers_stream.go, and responsesSSEDataPayload and
// responsesSSEDataLinesValid in
// sdk/api/handlers/openai/openai_responses_handlers.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Checks that each `data:` payload of a Responses event stream is JSON
//! before it reaches the client.

use open_ferry_translate::go;

/// How much of a bad payload an error quotes.
const PREVIEW_LIMIT: usize = 512;

/// Holds back an incomplete event until it can be checked.
#[derive(Debug, Default)]
pub(crate) struct SseCheck {
    pending: Vec<u8>,
    pending_err: Option<String>,
    prev_ends_with_cr: bool,
}

impl SseCheck {
    /// Adds a chunk of the stream and returns what can be sent on, with line
    /// endings made `\n`, or why the stream is broken. When a bad event
    /// follows good ones in the same chunk, the good ones come back first and
    /// the error with the next call.
    pub(crate) fn add_chunk(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        if let Some(err) = self.pending_err.take() {
            return Err(err);
        }
        let mut chunk = chunk;
        if chunk.is_empty() {
            return Ok(Vec::new());
        }
        if self.prev_ends_with_cr {
            if chunk[0] == b'\n' {
                chunk = &chunk[1..];
            }
            self.prev_ends_with_cr = false;
        }
        if chunk.is_empty() {
            return Ok(Vec::new());
        }
        let ends_with_cr = chunk.last() == Some(&b'\r');
        let chunk = normalize_newlines(chunk);
        self.prev_ends_with_cr = ends_with_cr;
        if !self.pending.is_empty() && !self.pending.ends_with(b"\n") && !chunk.starts_with(b"\n") {
            let first_line = chunk.split(|&b| b == b'\n').next().unwrap_or_default();
            let first_line = go::trim_space(first_line);
            if first_line.starts_with(b"data:") || first_line.starts_with(b"event:") {
                self.pending.push(b'\n');
            }
        }
        self.pending.extend_from_slice(&chunk);

        let mut output = Vec::new();
        while let Some(end) = self.pending.windows(2).position(|pair| pair == b"\n\n") {
            let end = end + 2;
            if let Err(err) = validate_frame(&self.pending[..end]) {
                if output.is_empty() {
                    return Err(err);
                }
                self.pending.clear();
                self.pending_err = Some(err);
                return Ok(output);
            }
            output.extend(self.pending.drain(..end));
        }

        if go::trim_space(&self.pending).is_empty() {
            self.pending.clear();
            return Ok(output);
        }
        // An incomplete event goes on now if its data is already whole.
        if data_lines_valid(&self.pending) {
            output.append(&mut self.pending);
        }
        Ok(output)
    }

    /// Checks what is left when the stream ends.
    pub(crate) fn finish(&mut self) -> Result<(), String> {
        self.prev_ends_with_cr = false;
        let pending = std::mem::take(&mut self.pending);
        if let Some(err) = self.pending_err.take() {
            return Err(err);
        }
        if go::trim_space(&pending).is_empty() {
            return Ok(());
        }
        validate_frame(&pending)
    }
}

/// `chunk` with each `\r\n` and lone `\r` made `\n`.
fn normalize_newlines(chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(chunk.len());
    let mut bytes = chunk.iter().peekable();
    while let Some(&b) = bytes.next() {
        if b == b'\r' {
            bytes.next_if_eq(&&b'\n');
            out.push(b'\n');
        } else {
            out.push(b);
        }
    }
    out
}

/// The `data:` lines of an event, each trimmed and joined with `\n`, or
/// `None` when it has none (`sseJSONValidationDataPayload`, and
/// `responsesSSEDataPayload`, which reads them the same way).
pub(crate) fn data_payload(frame: &[u8]) -> Option<Vec<u8>> {
    let mut payload: Option<Vec<u8>> = None;
    for line in frame.split(|&b| b == b'\n') {
        let Some(data) = go::trim_space(line).strip_prefix(b"data:") else {
            continue;
        };
        let payload = match &mut payload {
            Some(payload) => {
                payload.push(b'\n');
                payload
            }
            None => payload.insert(Vec::new()),
        };
        payload.extend_from_slice(go::trim_space(data));
    }
    payload
}

/// Whether an event's data, if it has any, may go to the client: nothing,
/// `[DONE]`, or JSON (`responsesSSEDataLinesValid`, and the check in
/// `validateSSEFrameDataJSON`).
pub(crate) fn data_lines_valid(frame: &[u8]) -> bool {
    let Some(payload) = data_payload(frame) else {
        return true;
    };
    let payload = go::trim_space(&payload);
    payload.is_empty() || payload == b"[DONE]" || go::json_valid(payload)
}

/// Checks one event (`validateSSEFrameDataJSON`).
fn validate_frame(frame: &[u8]) -> Result<(), String> {
    if data_lines_valid(frame) {
        return Ok(());
    }
    let payload = data_payload(frame).unwrap_or_default();
    let payload = go::trim_space(&payload);
    let preview = &payload[..payload.len().min(PREVIEW_LIMIT)];
    Err(format!(
        "invalid SSE data JSON (len={}): {}",
        payload.len(),
        go::quote_bytes(preview)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WANT: &str = "data: {\"type\":\"response.completed\",\ndata: \"response\":{\"status\":\"completed\"}}\n\n";

    fn run(chunks: &[&str]) -> Result<String, String> {
        let mut check = SseCheck::default();
        let mut output = Vec::new();
        for chunk in chunks {
            output.extend(check.add_chunk(chunk.as_bytes())?);
        }
        check.finish()?;
        Ok(String::from_utf8(output).unwrap())
    }

    #[test]
    fn allows_multiline_payloads() {
        let chunk = "event: response.completed\n\
                     data: {\"type\":\"response.completed\",\n\
                     data: \"response\":{\"status\":\"completed\"}}\n\n";
        assert_eq!(run(&[chunk]).unwrap(), chunk);
    }

    #[test]
    fn joins_split_line_endings() {
        let whole = "data: {\"type\":\"response.completed\",\r\ndata: \"response\":{\"status\":\"completed\"}}\r\n\r\n";
        assert_eq!(run(&[whole]).unwrap(), WANT);
        for chunks in [
            &[
                "data: {\"type\":\"response.completed\",\r",
                "\ndata: \"response\":{\"status\":\"completed\"}}\r\n\r\n",
            ][..],
            &[
                "data: {\"type\":\"response.completed\",\r",
                "",
                "",
                "\ndata: \"response\":{\"status\":\"completed\"}}\r\n\r\n",
            ],
            &[
                "data: {\"type\":\"response.completed\",\r",
                "\n",
                "data: \"response\":{\"status\":\"completed\"}}\r\n\r\n",
            ],
            &[
                "data: {\"type\":\"response.completed\",\r",
                "data: \"response\":{\"status\":\"completed\"}}\r\n\r\n",
            ],
        ] {
            assert_eq!(run(chunks).unwrap(), WANT, "{chunks:?}");
        }
    }

    #[test]
    fn rejoins_held_lines() {
        // The second chunk starts a new field, so a newline goes between.
        assert_eq!(
            run(&["data: {\"a\":", "data: 1}\n\n"]).unwrap(),
            "data: {\"a\":\ndata: 1}\n\n"
        );
        // A line that isn't held goes on as it is, as upstream sends it.
        assert_eq!(
            run(&["event: a", "data: {}\n\n"]).unwrap(),
            "event: adata: {}\n\n"
        );
        // Otherwise the chunks are one line.
        assert_eq!(
            run(&["data: {\"a\"", ":1}\n\n"]).unwrap(),
            "data: {\"a\":1}\n\n"
        );
    }

    #[test]
    fn sends_whole_data_before_the_event_ends() {
        let mut check = SseCheck::default();
        assert_eq!(check.add_chunk(b"data: {}").unwrap(), b"data: {}");
        assert_eq!(check.add_chunk(b"\n\n").unwrap(), b"\n\n");
        assert_eq!(check.add_chunk(b"data: [DONE]").unwrap(), b"data: [DONE]");
        assert_eq!(check.add_chunk(b"data: {\"a\":").unwrap(), b"");
        assert_eq!(check.add_chunk(b"1}").unwrap(), b"data: {\"a\":1}");
        check.finish().unwrap();
    }

    #[test]
    fn reports_bad_data() {
        assert_eq!(
            run(&["data: {\"a\":\n\n"]).unwrap_err(),
            "invalid SSE data JSON (len=5): \"{\\\"a\\\":\""
        );
        // An incomplete event is checked at the end.
        assert_eq!(
            run(&["data: nope"]).unwrap_err(),
            "invalid SSE data JSON (len=4): \"nope\""
        );
        let long = format!("data: {}é\n\n", "x".repeat(600));
        let err = run(&[&long]).unwrap_err();
        assert!(
            err.starts_with("invalid SSE data JSON (len=602): \"xxx"),
            "{err}"
        );
        assert_eq!(
            err.len(),
            "invalid SSE data JSON (len=602): \"\"".len() + 512
        );
    }

    #[test]
    fn sends_good_events_before_an_error() {
        let mut check = SseCheck::default();
        let out = check
            .add_chunk(b"data: {}\n\ndata: bad\n\ndata: {}\n\n")
            .unwrap();
        assert_eq!(out, b"data: {}\n\n");
        assert_eq!(
            check.add_chunk(b"data: {}\n\n").unwrap_err(),
            "invalid SSE data JSON (len=3): \"bad\""
        );
        let mut check = SseCheck::default();
        check.add_chunk(b"data: {}\n\ndata: bad\n\n").unwrap();
        assert!(check.finish().is_err());
        check.finish().unwrap();
    }

    #[test]
    fn invalid_bytes_are_quoted() {
        let mut check = SseCheck::default();
        assert_eq!(
            check.add_chunk(b"data: \xff\n\n").unwrap_err(),
            "invalid SSE data JSON (len=1): \"\\xff\""
        );
    }
}
