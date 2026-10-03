// Ported from CLIProxyAPI sseJSONValidationState in
// sdk/api/handlers/handlers_stream.go, and responsesSSEDataPayload and
// responsesSSEDataLinesValid in
// sdk/api/handlers/openai/openai_responses_handlers.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Checks that each `data:` payload of a Responses event stream is JSON
//! before it reaches the client.
//!
//! Deviations from upstream:
//! - An incomplete event is read on from where the last chunk left it, and
//!   whole events are cut from the held-back bytes in place, so a stream
//!   costs time in step with its size. Upstream reads the held-back event
//!   again, and moves what is left, for each chunk.
//! - An incomplete event may hold back at most [`MAX_EVENT_BYTES`]. One that
//!   grows past it ends the stream with an error, which the caller reports
//!   as a 502. Upstream holds it back without limit. When good events come
//!   before it in the same chunk, they go on and the error is ready at once,
//!   from [`SseCheck::take_overflow`], rather than with the next chunk as a
//!   bad event's error is.

mod scan;

pub(crate) use self::scan::EventScan;

use open_ferry_translate::go;

/// How much of a bad payload an error quotes.
const PREVIEW_LIMIT: usize = 512;

/// The most an incomplete event may hold back, here and in the Responses
/// handler, and the most that handler holds back before the first event it
/// can send. Upstream's executors read a provider's stream a line at a time,
/// with lines of up to 50 MB (`bufio.Scanner` given 52,428,800 bytes), and a
/// Responses event is in practice one such data line: no event upstream
/// would pass is bigger. The largest real events, `response.completed` with
/// every output item again (base64 images included), can still run to many
/// MiB, so the limit is generous: 64 MiB, the default request body limit.
pub(crate) const MAX_EVENT_BYTES: usize = 64 << 20;

/// How much an incomplete event may hold back: [`MAX_EVENT_BYTES`], unless a
/// test says otherwise.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EventLimit(pub(crate) usize);

impl Default for EventLimit {
    fn default() -> Self {
        Self(MAX_EVENT_BYTES)
    }
}

/// Why a stream stopped when an incomplete event grew past `limit`.
pub(crate) fn too_large(limit: EventLimit) -> String {
    format!("upstream SSE event exceeds {} bytes", limit.0)
}

/// How many bytes have been read in looking for whole events, counted in
/// tests only, which check that each byte is read about once.
#[derive(Debug, Default)]
pub(crate) struct Work(#[cfg(test)] pub(crate) usize);

impl Work {
    #[cfg_attr(not(test), allow(unused_variables, clippy::unused_self))]
    pub(crate) fn add(&mut self, read: usize) {
        #[cfg(test)]
        {
            self.0 += read;
        }
    }
}

/// Holds back an incomplete event until it can be checked.
#[derive(Debug, Default)]
pub(crate) struct SseCheck {
    pending: Vec<u8>,
    pending_err: Option<String>,
    /// Why the stream has to stop now, after the good events before an
    /// event that grew past the limit.
    overflow: Option<String>,
    prev_ends_with_cr: bool,
    /// Where a blank line ending an event may start in `pending`: no sooner
    /// than the byte before the last chunk.
    search: usize,
    /// What `pending` says so far.
    scan: EventScan,
    limit: EventLimit,
    work: Work,
}

impl SseCheck {
    /// A check that holds back at most `limit` bytes of an event.
    #[cfg(test)]
    pub(crate) fn with_limit(limit: usize) -> Self {
        Self {
            limit: EventLimit(limit),
            ..Self::default()
        }
    }

    /// Adds a chunk of the stream and returns what can be sent on, with line
    /// endings made `\n`, or why the stream is broken. When a bad event
    /// follows good ones in the same chunk, the good ones come back first and
    /// the error with the next call. An incomplete event that has grown past
    /// the limit is an error too, which, after good events, is ready at once
    /// from [`SseCheck::take_overflow`].
    pub(crate) fn add_chunk(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        if let Some(err) = self.pending_err.take().or_else(|| self.overflow.take()) {
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

        // Each whole event is checked where it lies, and what is left is
        // moved down once.
        let mut output = Vec::new();
        let mut start = 0;
        while let Some(end) = self.next_frame_end() {
            let frame = self.pending.get(start..end).unwrap_or_default();
            self.work.add(self.scan.advance(frame));
            if !self.scan.data_valid() {
                let err = invalid_data(frame);
                if output.is_empty() {
                    return Err(err);
                }
                self.reset();
                self.pending_err = Some(err);
                return Ok(output);
            }
            output.extend_from_slice(frame);
            self.scan = EventScan::default();
            start = end;
            self.search = end;
        }
        self.pending.drain(..start);
        self.search = self.pending.len().saturating_sub(1);

        self.work.add(self.scan.advance(&self.pending));
        if self.scan.is_blank() {
            self.reset();
            return Ok(output);
        }
        // An incomplete event goes on now if its data is already whole.
        if self.scan.data_valid() {
            output.append(&mut self.pending);
            self.reset();
            return Ok(output);
        }
        if self.pending.len() > self.limit.0 {
            let err = too_large(self.limit);
            self.reset();
            if output.is_empty() {
                return Err(err);
            }
            self.overflow = Some(err);
        }
        Ok(output)
    }

    /// Why the stream has to stop before its next chunk is read: an event
    /// grew past the limit after the good events last returned.
    pub(crate) fn take_overflow(&mut self) -> Option<String> {
        self.overflow.take()
    }

    /// The end of the next whole event in `pending`, just past its blank
    /// line. The search starts where the last one stopped.
    fn next_frame_end(&mut self) -> Option<usize> {
        let rest = self.pending.get(self.search..).unwrap_or_default();
        let found = rest.windows(2).position(|pair| pair == b"\n\n");
        self.work.add(found.map_or(rest.len(), |at| at + 2));
        Some(self.search + found? + 2)
    }

    /// Drops what is held back.
    fn reset(&mut self) {
        self.pending.clear();
        self.search = 0;
        self.scan = EventScan::default();
    }

    /// Checks what is left when the stream ends.
    pub(crate) fn finish(&mut self) -> Result<(), String> {
        self.prev_ends_with_cr = false;
        let pending = std::mem::take(&mut self.pending);
        self.reset();
        if let Some(err) = self.pending_err.take().or_else(|| self.overflow.take()) {
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
    Err(invalid_data(frame))
}

/// Why an event whose data isn't JSON stopped the stream.
fn invalid_data(frame: &[u8]) -> String {
    let payload = data_payload(frame).unwrap_or_default();
    let payload = go::trim_space(&payload);
    let preview = &payload[..payload.len().min(PREVIEW_LIMIT)];
    format!(
        "invalid SSE data JSON (len={}): {}",
        payload.len(),
        go::quote_bytes(preview)
    )
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

    /// Upstream's `AddChunk` and `Finish`, as this read them before it read
    /// on from where it left off, to check against.
    #[derive(Default)]
    struct Upstream {
        pending: Vec<u8>,
        pending_err: Option<String>,
        prev_ends_with_cr: bool,
    }

    impl Upstream {
        fn add_chunk(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
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
            if !self.pending.is_empty()
                && !self.pending.ends_with(b"\n")
                && !chunk.starts_with(b"\n")
            {
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
            if data_lines_valid(&self.pending) {
                output.append(&mut self.pending);
            }
            Ok(output)
        }

        fn finish(&mut self) -> Result<(), String> {
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
    // each chunk, over streams cut at random, up to the first error.
    #[test]
    fn checks_as_upstream_does() {
        let tokens: &[&[u8]] = &[
            b"data:",
            b"data: ",
            b"data: ",
            b"event: x",
            b"da",
            b"ta:",
            b"ev",
            b": c",
            b"id: 1",
            b"\n",
            b"\n",
            b"\n",
            b"\n\n",
            b"\r",
            b"\r\n",
            b"\r\n\r\n",
            b" ",
            b"\t",
            b"\xc2\xa0",
            b"\xe3\x80\x80",
            b"\xe3\x80",
            b"\xff",
            b"{",
            b"}",
            b"[",
            b"]",
            b"\"",
            b"\"k\":",
            b"\"v\"",
            b",",
            b"1",
            b"null",
            b"[DONE]",
            b"{}",
            b"{\"a\":1}",
            b"x",
        ];
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..20_000 {
            let mut stream = Vec::new();
            for _ in 0..rng.below(30) {
                stream.extend_from_slice(tokens[rng.below(tokens.len())]);
            }
            let mut upstream = Upstream::default();
            let mut check = SseCheck::default();
            let mut at = 0;
            let mut failed = false;
            while at < stream.len() && !failed {
                let end = (at + 1 + rng.below(8)).min(stream.len());
                let want = upstream.add_chunk(&stream[at..end]);
                let got = check.add_chunk(&stream[at..end]);
                assert_eq!(got, want, "{:?} cut at {end}", go::quote_bytes(&stream));
                failed = want.is_err();
                at = end;
            }
            if !failed {
                assert_eq!(
                    check.finish(),
                    upstream.finish(),
                    "{:?}",
                    go::quote_bytes(&stream)
                );
            }
        }
    }

    /// An event whose data is whole only with its last three bytes, `}` and
    /// a blank line, with `len` bytes of text in its data.
    fn event_of(len: usize) -> Vec<u8> {
        let mut event = b"data: {\"delta\":\"".to_vec();
        event.resize(event.len() + len, b'x');
        event.extend_from_slice(b"\"}\n\n");
        event
    }

    #[test]
    fn stops_an_event_past_the_limit() {
        let mut check = SseCheck::with_limit(64);
        assert_eq!(check.add_chunk(b"data: {\"delta\":\"").unwrap(), b"");
        assert_eq!(check.add_chunk(&[b'x'; 48]).unwrap(), b"");
        assert_eq!(check.pending.len(), 64);
        assert_eq!(
            check.add_chunk(b"x").unwrap_err(),
            "upstream SSE event exceeds 64 bytes"
        );
        assert!(check.pending.is_empty());

        // After good events, they go on and the error is ready at once.
        let mut check = SseCheck::with_limit(64);
        let mut chunk = b"data: {}\n\n".to_vec();
        chunk.extend_from_slice(&event_of(100)[..100]);
        assert_eq!(check.add_chunk(&chunk).unwrap(), b"data: {}\n\n");
        assert!(check.pending.is_empty());
        assert_eq!(
            check.take_overflow().unwrap(),
            "upstream SSE event exceeds 64 bytes"
        );
        assert_eq!(check.take_overflow(), None);
        // Untaken, it is the next call's error.
        let mut check = SseCheck::with_limit(64);
        check.add_chunk(&chunk).unwrap();
        assert!(check.add_chunk(b"data: {}\n\n").is_err());
        let mut check = SseCheck::with_limit(64);
        check.add_chunk(&chunk).unwrap();
        assert!(check.finish().is_err());
    }

    #[test]
    fn passes_an_event_at_the_limit() {
        let event = event_of(1000);
        // Held back, the event is at most all but its last three bytes.
        let (held, rest) = event.split_at(event.len() - 3);
        let mut check = SseCheck::with_limit(held.len());
        assert_eq!(check.add_chunk(held).unwrap(), b"");
        assert_eq!(check.add_chunk(rest).unwrap(), event);
        check.finish().unwrap();

        let mut check = SseCheck::with_limit(held.len() - 1);
        assert_eq!(
            check.add_chunk(held).unwrap_err(),
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
        let event = event_of(6 << 20);
        let mut check = SseCheck::default();
        let mut output = Vec::new();
        let mut fed = 0;
        for chunk in event.chunks(1024) {
            output.extend(check.add_chunk(chunk).unwrap());
            fed += chunk.len();
            assert!(check.work.0 <= 3 * fed, "{} after {fed}", check.work.0);
            if output.is_empty() {
                assert_eq!(check.pending.len(), fed);
                assert!(check.pending.capacity() <= 2 * fed + 1024);
            }
        }
        check.finish().unwrap();
        assert_eq!(output, event);

        // An event too large fails as soon as it is, still in linear time.
        let mut check = SseCheck::with_limit(1 << 20);
        let mut failed = None;
        let mut fed = 0;
        for (i, chunk) in event.chunks(1024).enumerate() {
            let result = check.add_chunk(chunk);
            fed += chunk.len();
            assert!(check.work.0 <= 3 * fed, "{} after {fed}", check.work.0);
            match result {
                Ok(out) => assert!(out.is_empty()),
                Err(_) => {
                    failed = Some(i);
                    break;
                }
            }
            assert!(check.pending.len() <= 1 << 20);
        }
        assert_eq!(failed, Some(1024));
    }
}
