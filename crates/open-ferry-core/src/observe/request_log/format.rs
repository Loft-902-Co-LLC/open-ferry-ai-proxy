// Ported from CLIProxyAPI internal/logging/request_logger_format.go
// (writeNonStreamingLog, writeRequestInfoWithBody, writeAPISection,
// writeAPIErrorResponses, writeResponseSection, countTrailingNewlinesBytes,
// writeSectionSpacing, inferDownstreamTransport, inferUpstreamTransport,
// decompressResponse), internal/logging/request_logger_streaming.go
// (writeFinalLog), internal/api/middleware/request_logging.go
// (decodeCapturedRequestBodyForLogWithLimit) and
// internal/runtime/executor/helps/logging_helpers.go (writeHeaders)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! How a request log reads: the sections of a log file, byte for byte as
//! upstream writes them, and the decoding of the bodies it shows.
//!
//! A log has the client's request (`=== REQUEST INFO ===`, `=== HEADERS ===`
//! and `=== REQUEST BODY ===`), the upstream WebSocket timeline, the
//! upstream requests and responses of each attempt, the upstream errors the
//! handlers recorded, and the answer (`=== RESPONSE ===`). Sections are
//! kept apart by blank lines, so that each one ends with two of them.
//!
//! Deviations from upstream:
//! - Header lines are sorted by name, written as Go writes a header name
//!   (`Content-Type`), where upstream writes the client's headers and the
//!   answer's in map order.
//! - Every credential header's value is masked, the answer's included (see
//!   [`mask::mask_header_value`]); upstream masks only the client's
//!   request headers and the upstream requests', and fewer names.
//! - A decoded answer is cut at a limit (see [`DECODE_LIMIT`]); the texts
//!   of the decoding errors are Rust's.
//! - A compressed request body is decoded for `gzip`, `deflate`, `br` and
//!   `zstd`, to at most a limit, and one that can't be decoded, has an
//!   encoding not known here, or decodes past the limit is shown as a
//!   one-line placeholder ([`decode_request_body`]), never as it came,
//!   since a secret compressed in it couldn't be scrubbed. Upstream decodes
//!   only `zstd`, shows any other body as it came, and cuts one past its
//!   limit with a marker.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::Read;

use chrono::{DateTime, Offset, TimeZone, Timelike};
use http::HeaderMap;
use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

use super::ApiError;
use crate::observe::mask;

/// The most bytes a body decoded for a log keeps (upstream's
/// `maxDeferredErrorRequestBodyBytes`, which upstream applies to a deferred
/// request body only).
pub(crate) const DECODE_LIMIT: usize = 32 << 20;

/// What a log shows for a compressed request body it can't show decoded,
/// with the reason.
fn omitted(reason: &str) -> Vec<u8> {
    format!("[ENCODED REQUEST BODY OMITTED: {reason}]").into_bytes()
}

/// Go's `CanonicalMIMEHeaderKey`: each `-`-separated word of `name` with
/// its first letter upper case and the rest lower case, or `name` as it is
/// when it isn't a valid header name.
pub(crate) fn canonical_header_key(name: &str) -> String {
    if !name.bytes().all(is_token_byte) {
        return name.to_owned();
    }
    let mut upper = true;
    name.chars()
        .map(|c| {
            let mapped = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            mapped
        })
        .collect()
}

/// Whether `b` may be in a header name (Go's `validHeaderFieldByte`).
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// `time` as Go formats it with `time.RFC3339Nano`: the fraction of the
/// second without trailing zeros, and `Z` for UTC.
pub(crate) fn rfc3339_nano<Tz: TimeZone>(time: &DateTime<Tz>) -> String
where
    Tz::Offset: fmt::Display,
{
    let mut out = time.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = time.nanosecond() % 1_000_000_000;
    if nanos != 0 {
        let digits = format!("{nanos:09}");
        out.push('.');
        out.push_str(digits.trim_end_matches('0'));
    }
    if time.offset().fix().local_minus_utc() == 0 {
        out.push('Z');
    } else {
        let _ = write!(out, "{}", time.format("%:z"));
    }
    out
}

/// `headers` by name, each name as Go writes it, sorted, with its values
/// in order.
fn sorted_headers(headers: &HeaderMap) -> BTreeMap<String, Vec<String>> {
    let mut sorted: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        sorted
            .entry(canonical_header_key(name.as_str()))
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    sorted
}

/// Writes `key: value` lines for `headers`, each credential masked.
fn write_header_lines(out: &mut String, headers: &HeaderMap) {
    for (key, values) in sorted_headers(headers) {
        for value in values {
            let _ = writeln!(out, "{key}: {}", mask::mask_header_value(&key, &value));
        }
    }
}

/// The headers of an upstream request or answer as the API sections show
/// them (upstream's `writeHeaders`): `<none>` when there are none.
pub(crate) fn write_headers(out: &mut String, headers: &HeaderMap) {
    if headers.is_empty() {
        out.push_str("<none>\n");
        return;
    }
    write_header_lines(out, headers);
}

/// How many `\n` `payload` ends with.
pub(crate) fn count_trailing_newlines(payload: &[u8]) -> usize {
    payload.iter().rev().take_while(|&&b| b == b'\n').count()
}

/// Ends a section that ends with `trailing` newlines with enough more to
/// make three (upstream's `writeSectionSpacing`).
fn section_spacing(out: &mut Vec<u8>, trailing: usize) {
    for _ in trailing..3 {
        out.push(b'\n');
    }
}

/// Whether `payload` has anything but white space (upstream's
/// `hasSectionPayload`).
fn has_payload(payload: &[u8]) -> bool {
    !payload.trim_ascii().is_empty()
}

/// The upstream transports a log shows (upstream's
/// `inferUpstreamTransport`).
fn upstream_transport(api_request: &[u8], api_response: &[u8], api_ws: &[u8]) -> &'static str {
    let http = has_payload(api_request) || has_payload(api_response);
    let ws = has_payload(api_ws);
    match (http, ws) {
        (true, true) => "websocket+http",
        (false, true) => "websocket",
        (true, false) => "http",
        (false, false) => "",
    }
}

/// The client's transport (upstream's `inferDownstreamTransport`, without
/// a downstream WebSocket timeline): `websocket` for an upgrade.
fn downstream_transport(headers: &HeaderMap) -> &'static str {
    let upgrade = headers.get_all(http::header::UPGRADE).iter().any(|value| {
        value
            .as_bytes()
            .trim_ascii()
            .eq_ignore_ascii_case(b"websocket")
    });
    if upgrade { "websocket" } else { "http" }
}

/// What a request log is made of.
pub(crate) struct Sections<'a> {
    /// The client's URL, its query masked.
    pub url: &'a str,
    /// The client's method.
    pub method: &'a str,
    /// The client's headers.
    pub headers: &'a HeaderMap,
    /// The client's body, decoded.
    pub body: &'a [u8],
    /// The time of the `=== REQUEST INFO ===` section.
    pub timestamp: String,
    /// The upstream WebSocket timeline.
    pub api_ws_timeline: &'a [u8],
    /// The upstream requests.
    pub api_request: &'a [u8],
    /// The upstream errors the handlers recorded.
    pub api_errors: &'a [ApiError],
    /// The upstream responses.
    pub api_response: &'a [u8],
    /// The answer's status.
    pub status: u16,
    /// The answer's headers.
    pub response_headers: &'a HeaderMap,
    /// The answer's body, as it was sent.
    pub response: &'a [u8],
}

/// Writes the `=== REQUEST INFO ===`, `=== HEADERS ===` and
/// `=== REQUEST BODY ===` sections (upstream's `writeRequestInfoWithBody`).
fn write_request_info(out: &mut Vec<u8>, sections: &Sections<'_>, downstream: &str) {
    let mut info = String::new();
    info.push_str("=== REQUEST INFO ===\n");
    let _ = writeln!(info, "Version: {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(info, "URL: {}", sections.url);
    let _ = writeln!(info, "Method: {}", sections.method);
    let _ = writeln!(info, "Downstream Transport: {downstream}");
    let upstream = upstream_transport(
        sections.api_request,
        sections.api_response,
        sections.api_ws_timeline,
    );
    if !upstream.is_empty() {
        let _ = writeln!(info, "Upstream Transport: {upstream}");
    }
    let _ = writeln!(info, "Timestamp: {}", sections.timestamp);
    out.extend_from_slice(info.as_bytes());
    section_spacing(out, 1);

    let mut headers = String::from("=== HEADERS ===\n");
    write_header_lines(&mut headers, sections.headers);
    out.extend_from_slice(headers.as_bytes());
    section_spacing(out, 1);

    out.extend_from_slice(b"=== REQUEST BODY ===\n");
    out.extend_from_slice(sections.body);
    let trailing = if sections.body.is_empty() {
        1
    } else {
        count_trailing_newlines(sections.body)
    };
    section_spacing(out, trailing);
}

/// Writes an API section: nothing when `payload` is empty, `payload` as it
/// is when it starts with `prefix`, else `header` and `payload` (upstream's
/// `writeAPISection`).
fn write_api_section(out: &mut Vec<u8>, header: &str, prefix: &str, payload: &[u8]) {
    if payload.is_empty() {
        return;
    }
    if !payload.starts_with(prefix.as_bytes()) {
        out.extend_from_slice(header.as_bytes());
    }
    out.extend_from_slice(payload);
    section_spacing(out, count_trailing_newlines(payload));
}

/// Writes an `=== API ERROR RESPONSE ===` section for each error (upstream's
/// `writeAPIErrorResponses`).
fn write_api_errors(out: &mut Vec<u8>, errors: &[ApiError]) {
    for error in errors {
        out.extend_from_slice(b"=== API ERROR RESPONSE ===\n");
        out.extend_from_slice(format!("HTTP Status: {}\n", error.status).as_bytes());
        out.extend_from_slice(error.message.as_bytes());
        let trailing = if error.message.is_empty() {
            1
        } else {
            count_trailing_newlines(error.message.as_bytes())
        };
        section_spacing(out, trailing);
    }
}

/// Writes the `=== RESPONSE ===` section (upstream's
/// `writeResponseSection`, the status always written).
fn write_response_section(
    out: &mut Vec<u8>,
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
    decode_error: Option<&str>,
    trailing_newline: bool,
) {
    let mut head = format!("=== RESPONSE ===\nStatus: {status}\n");
    write_header_lines(&mut head, headers);
    out.extend_from_slice(head.as_bytes());
    if !(body.starts_with(b"\n") || body.starts_with(b"\r\n")) {
        out.push(b'\n');
    }
    out.extend_from_slice(body);
    if let Some(error) = decode_error {
        out.extend_from_slice(format!("\n[DECOMPRESSION ERROR: {error}]").as_bytes());
    }
    if trailing_newline {
        out.push(b'\n');
    }
}

/// The log of a request whose answer was not a stream (upstream's
/// `writeNonStreamingLog`): its answer is shown decoded, after the
/// upstream errors.
pub(crate) fn non_streaming(sections: &Sections<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    write_request_info(&mut out, sections, downstream_transport(sections.headers));
    write_api_section(
        &mut out,
        "=== API WEBSOCKET TIMELINE ===\n",
        "=== API WEBSOCKET TIMELINE",
        sections.api_ws_timeline,
    );
    write_api_section(
        &mut out,
        "=== API REQUEST ===\n",
        "=== API REQUEST",
        sections.api_request,
    );
    write_api_errors(&mut out, sections.api_errors);
    write_api_section(
        &mut out,
        "=== API RESPONSE ===\n",
        "=== API RESPONSE",
        sections.api_response,
    );
    let (body, error) = decompress_response(sections.response_headers, sections.response);
    write_response_section(
        &mut out,
        sections.status,
        sections.response_headers,
        &body,
        error.as_deref(),
        true,
    );
    out
}

/// The log of a request answered with a stream (upstream's
/// `writeFinalLog`): its answer is shown as it was sent, and the upstream
/// errors are left out.
pub(crate) fn streaming(sections: &Sections<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    write_request_info(&mut out, sections, "http");
    write_api_section(
        &mut out,
        "=== API WEBSOCKET TIMELINE ===\n",
        "=== API WEBSOCKET TIMELINE",
        sections.api_ws_timeline,
    );
    write_api_section(
        &mut out,
        "=== API REQUEST ===\n",
        "=== API REQUEST",
        sections.api_request,
    );
    write_api_section(
        &mut out,
        "=== API RESPONSE ===\n",
        "=== API RESPONSE",
        sections.api_response,
    );
    write_response_section(
        &mut out,
        sections.status,
        sections.response_headers,
        sections.response,
        None,
        false,
    );
    out
}

/// Whether an answer with `content_type` to a request with `body` is a
/// stream (upstream's `detectStreaming`): an event stream, or, when the
/// answer names no type, a request asking for a stream.
pub(crate) fn is_streaming(content_type: Option<&str>, body: &[u8]) -> bool {
    match content_type {
        Some(content_type) if content_type.contains("text/event-stream") => true,
        Some(content_type) if !content_type.trim().is_empty() => false,
        _ => contains(body, b"\"stream\": true") || contains(body, b"\"stream\":true"),
    }
}

/// Whether `haystack` holds `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The answer's body decoded as its first `Content-Encoding` says, and
/// the error when it can't be (upstream's `decompressResponse`); the body
/// as it is on an error, or for an encoding it doesn't know.
pub(crate) fn decompress_response<'a>(
    headers: &HeaderMap,
    body: &'a [u8],
) -> (Cow<'a, [u8]>, Option<String>) {
    if headers.is_empty() || body.is_empty() {
        return (Cow::Borrowed(body), None);
    }
    let encoding = headers
        .get(http::header::CONTENT_ENCODING)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).to_lowercase())
        .unwrap_or_default();
    let decoded = match encoding.as_str() {
        "gzip" => read_limited(
            flate2::read::MultiGzDecoder::new(body),
            "gzip",
            DECODE_LIMIT,
        ),
        "deflate" => read_limited(
            flate2::read::DeflateDecoder::new(body),
            "deflate",
            DECODE_LIMIT,
        ),
        "br" => read_limited(
            brotli_decompressor::Decompressor::new(body, 4096),
            "brotli",
            DECODE_LIMIT,
        ),
        "zstd" => decode_zstd(body, DECODE_LIMIT)
            .map_err(|error| format!("failed to decompress zstd data: {error}")),
        _ => return (Cow::Borrowed(body), None),
    };
    match decoded {
        Ok((decoded, false)) => (Cow::Owned(decoded), None),
        Ok((decoded, true)) => (
            Cow::Owned(decoded),
            Some(format!(
                "decompressed body is over {DECODE_LIMIT} bytes; the rest is left out"
            )),
        ),
        Err(error) => (Cow::Borrowed(body), Some(error)),
    }
}

/// Reads `reader` to its end or to `limit` bytes, saying whether there was
/// more.
fn read_limited(reader: impl Read, name: &str, limit: usize) -> Result<(Vec<u8>, bool), String> {
    let mut out = Vec::new();
    reader
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|error| format!("failed to decompress {name} data: {error}"))?;
    let truncated = out.len() > limit;
    out.truncate(limit);
    Ok((out, truncated))
}

/// Decodes an HTTP `deflate` body, zlib-wrapped as the standard says or
/// raw as some clients send it, to at most `limit` bytes, saying whether
/// there was more.
fn decode_deflate(body: &[u8], limit: usize) -> Result<(Vec<u8>, bool), String> {
    read_limited(flate2::read::ZlibDecoder::new(body), "deflate", limit)
        .or_else(|_| read_limited(flate2::read::DeflateDecoder::new(body), "deflate", limit))
}

/// Decodes a zstd stream of any number of frames, skipping skippable ones,
/// to at most `limit` bytes, saying whether there was more.
fn decode_zstd(mut input: &[u8], limit: usize) -> Result<(Vec<u8>, bool), String> {
    let mut decoder = FrameDecoder::new();
    let mut output = Vec::new();
    while !input.is_empty() {
        match decoder.init(&mut input) {
            Ok(()) => {}
            Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                length,
                ..
            })) => {
                input = usize::try_from(length)
                    .ok()
                    .and_then(|length| input.get(length..))
                    .ok_or("truncated skippable frame")?;
                continue;
            }
            Err(error) => return Err(error.to_string()),
        }
        loop {
            decoder
                .decode_blocks(&mut input, BlockDecodingStrategy::UptoBytes(1 << 20))
                .map_err(|error| error.to_string())?;
            if let Some(chunk) = decoder.collect() {
                let room = limit.saturating_sub(output.len());
                if chunk.len() > room {
                    output.extend_from_slice(chunk.get(..room).unwrap_or_default());
                    return Ok((output, true));
                }
                output.extend_from_slice(&chunk);
            }
            if decoder.is_finished() {
                break;
            }
        }
        if let Some(expected) = decoder.get_checksum_from_data()
            && decoder.get_calculated_checksum() != Some(expected)
        {
            return Err("checksum mismatch".to_owned());
        }
    }
    Ok((output, false))
}

/// A request body as a log shows it, decoded as `encoding` says to at most
/// `limit` bytes (upstream's `decodeCapturedRequestBodyForLogWithLimit`):
/// `gzip`, `deflate`, `br` and `zstd` are decoded, in the reverse of the
/// order they were applied. A body that can't be decoded, has another
/// encoding, or decodes past `limit` is replaced by a one-line placeholder,
/// never kept as it came.
pub(crate) fn decode_request_body<'a>(
    raw: &'a [u8],
    encoding: &str,
    limit: usize,
) -> Cow<'a, [u8]> {
    let encoding = encoding.trim();
    if raw.is_empty() || encoding.is_empty() || encoding.eq_ignore_ascii_case("identity") {
        return Cow::Borrowed(raw);
    }
    let mut body = Cow::Borrowed(raw);
    for part in encoding.rsplit(',') {
        let decoded = match part.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => continue,
            "gzip" | "x-gzip" => {
                read_limited(flate2::read::MultiGzDecoder::new(&*body), "gzip", limit)
            }
            "deflate" => decode_deflate(&body, limit),
            "br" => read_limited(
                brotli_decompressor::Decompressor::new(&*body, 4096),
                "brotli",
                limit,
            ),
            "zstd" => decode_zstd(&body, limit),
            _ => return Cow::Owned(omitted("its Content-Encoding isn't supported")),
        };
        body = match decoded {
            Ok((decoded, false)) => Cow::Owned(decoded),
            Ok((_, true)) => {
                return Cow::Owned(omitted(&format!("it decodes to over {limit} bytes")));
            }
            Err(_) => return Cow::Owned(omitted("it couldn't be decoded")),
        };
    }
    body
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use chrono::{FixedOffset, Utc};
    use http::HeaderValue;

    use super::*;

    fn zstd(payload: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(payload, ruzstd::encoding::CompressionLevel::Fastest)
    }

    // Not upstream's: Go's time.RFC3339Nano.
    #[test]
    fn formats_times_as_go_does() {
        let utc = Utc.with_ymd_and_hms(2026, 9, 23, 12, 0, 5).unwrap();
        assert_eq!(rfc3339_nano(&utc), "2026-09-23T12:00:05Z");
        let fraction = utc + chrono::Duration::nanoseconds(120_000_000);
        assert_eq!(rfc3339_nano(&fraction), "2026-09-23T12:00:05.12Z");
        let east = FixedOffset::east_opt(2 * 3600 + 30 * 60).unwrap();
        let local = utc.with_timezone(&east) + chrono::Duration::nanoseconds(1);
        assert_eq!(rfc3339_nano(&local), "2026-09-23T14:30:05.000000001+02:30");
    }

    // Not upstream's: Go's header names.
    #[test]
    fn writes_header_names_as_go_does() {
        assert_eq!(canonical_header_key("content-type"), "Content-Type");
        assert_eq!(canonical_header_key("x-api-KEY"), "X-Api-Key");
        assert_eq!(canonical_header_key("x--y"), "X--Y");
        let mut headers = HeaderMap::new();
        headers.append("x-b", HeaderValue::from_static("2"));
        headers.append(
            "authorization",
            HeaderValue::from_static("Bearer sk-1234567890"),
        );
        headers.append("x-b", HeaderValue::from_static("1"));
        let mut out = String::new();
        write_headers(&mut out, &headers);
        assert_eq!(out, "Authorization: Bearer sk-1...7890\nX-B: 2\nX-B: 1\n");
        let mut none = String::new();
        write_headers(&mut none, &HeaderMap::new());
        assert_eq!(none, "<none>\n");
    }

    fn sections<'a>(headers: &'a HeaderMap, response_headers: &'a HeaderMap) -> Sections<'a> {
        Sections {
            url: "/v1/chat/completions",
            method: "POST",
            headers,
            body: b"{\"a\":1}",
            timestamp: "2026-09-23T12:00:05Z".to_owned(),
            api_ws_timeline: b"",
            api_request: b"=== API REQUEST 1 ===\nbody\n\n",
            api_errors: &[],
            api_response: b"=== API RESPONSE 1 ===\nok\n",
            status: 200,
            response_headers,
            response: b"done",
        }
    }

    // Not upstream's: the sections of a log, in upstream's order and
    // spacing.
    #[test]
    fn writes_a_non_streaming_log() {
        let headers = HeaderMap::new();
        let mut response_headers = HeaderMap::new();
        response_headers.insert("content-type", HeaderValue::from_static("application/json"));
        let errors = [ApiError {
            status: 502,
            message: "bad gateway".to_owned(),
            canceled: false,
        }];
        let mut sections = sections(&headers, &response_headers);
        sections.api_errors = &errors;
        let log = String::from_utf8(non_streaming(&sections)).unwrap();
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(
            log,
            format!(
                "=== REQUEST INFO ===\nVersion: {version}\nURL: /v1/chat/completions\n\
                 Method: POST\nDownstream Transport: http\nUpstream Transport: http\n\
                 Timestamp: 2026-09-23T12:00:05Z\n\n\n=== HEADERS ===\n\n\n\
                 === REQUEST BODY ===\n{{\"a\":1}}\n\n\n\
                 === API REQUEST 1 ===\nbody\n\n\n\
                 === API ERROR RESPONSE ===\nHTTP Status: 502\nbad gateway\n\n\n\
                 === API RESPONSE 1 ===\nok\n\n\n\
                 === RESPONSE ===\nStatus: 200\nContent-Type: application/json\n\ndone\n"
            )
        );
    }

    // Not upstream's: a streamed answer is written as it was sent, without
    // the upstream errors or a final newline.
    #[test]
    fn writes_a_streaming_log() {
        let mut headers = HeaderMap::new();
        headers.insert("upgrade", HeaderValue::from_static(" WebSocket "));
        let response_headers = HeaderMap::new();
        let mut sections = sections(&headers, &response_headers);
        sections.api_ws_timeline = b"Timestamp: x\nEvent: api.websocket.request\n";
        sections.response = b"\ndata: 1\n\n";
        let log = String::from_utf8(streaming(&sections)).unwrap();
        assert!(log.contains("Downstream Transport: http\nUpstream Transport: websocket+http\n"));
        assert!(log.contains(
            "=== API WEBSOCKET TIMELINE ===\nTimestamp: x\nEvent: api.websocket.request\n\n\n\
             === API REQUEST 1 ==="
        ));
        assert!(
            log.ends_with("=== RESPONSE ===\nStatus: 200\n\ndata: 1\n\n"),
            "{log:?}"
        );
        let log = String::from_utf8(non_streaming(&sections)).unwrap();
        assert!(log.contains("Downstream Transport: websocket\n"));
    }

    // Not upstream's: a compressed answer is shown decoded, and one that
    // can't be decoded as it came, with the error.
    #[test]
    fn decompresses_answers() {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(b"hello").unwrap();
        let gzip = gzip.finish().unwrap();
        let (body, error) = decompress_response(&headers, &gzip);
        assert_eq!((&*body, error), (&b"hello"[..], None));

        headers.insert("content-encoding", HeaderValue::from_static("ZSTD"));
        let compressed = zstd(b"hi");
        let (body, error) = decompress_response(&headers, &compressed);
        assert_eq!((&*body, error), (&b"hi"[..], None));

        headers.insert("content-encoding", HeaderValue::from_static("deflate"));
        let (body, error) = decompress_response(&headers, b"not deflate at all");
        assert_eq!(&*body, b"not deflate at all");
        assert!(
            error
                .unwrap()
                .starts_with("failed to decompress deflate data: ")
        );

        headers.insert("content-encoding", HeaderValue::from_static("identity"));
        let (body, error) = decompress_response(&headers, b"plain");
        assert_eq!((&*body, error), (&b"plain"[..], None));
    }

    /// `input` as a brotli stream: one uncompressed meta-block, then an
    /// empty last one.
    fn brotli(input: &[u8]) -> Vec<u8> {
        // Bits, lowest first: WBITS 16 (0), then the meta-block's header:
        // ISLAST 0, MNIBBLES 4 (00), MLEN - 1 in 16 bits, ISUNCOMPRESSED 1;
        // 21 bits, padded to three bytes. The last meta-block is ISLAST 1,
        // ISLASTEMPTY 1.
        let len = u32::try_from(input.len() - 1).unwrap();
        let bits = (len << 4) | (1 << 20);
        let mut out = bits.to_le_bytes()[..3].to_vec();
        out.extend_from_slice(input);
        out.push(0b11);
        out
    }

    // Not upstream's: gzip, deflate, br and zstd request bodies are
    // decoded, stacked ones too; one that can't be, or with another
    // encoding, is never shown as it came.
    #[test]
    fn decodes_request_bodies() {
        let json = b"{\"a\":1}";
        let body = zstd(json);
        assert_eq!(&*decode_request_body(&body, " zstd ", 1 << 20), json);
        assert_eq!(
            &*decode_request_body(&body, "identity, zstd", 1 << 20),
            json
        );
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(json).unwrap();
        let gzip = gzip.finish().unwrap();
        assert_eq!(&*decode_request_body(&gzip, "GZIP", 1 << 20), json);
        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        zlib.write_all(json).unwrap();
        let zlib = zlib.finish().unwrap();
        assert_eq!(&*decode_request_body(&zlib, "deflate", 1 << 20), json);
        let mut raw = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        raw.write_all(json).unwrap();
        let raw = raw.finish().unwrap();
        assert_eq!(&*decode_request_body(&raw, "deflate", 1 << 20), json);
        assert_eq!(&*decode_request_body(&brotli(json), "br", 1 << 20), json);
        assert_eq!(
            &*decode_request_body(&zstd(&gzip), "gzip, zstd", 1 << 20),
            json
        );

        let undecodable = b"[ENCODED REQUEST BODY OMITTED: it couldn't be decoded]";
        assert_eq!(&*decode_request_body(&body, "gzip", 1 << 20), undecodable);
        assert_eq!(&*decode_request_body(b"junk", "zstd", 1 << 20), undecodable);
        assert_eq!(
            &*decode_request_body(&gzip[..gzip.len() - 4], "gzip", 1 << 20),
            undecodable
        );
        assert_eq!(
            &*decode_request_body(&body, "compress", 1 << 20),
            b"[ENCODED REQUEST BODY OMITTED: its Content-Encoding isn't supported]"
        );
        assert_eq!(&*decode_request_body(json, "", 1 << 20), json);
        assert_eq!(&*decode_request_body(json, "identity", 1 << 20), json);
    }

    // Ports TestDecodeCapturedRequestBodyForLogWithLimitTruncatesZstdExpansion,
    // except that a body decoding past the limit is replaced by a
    // placeholder, where upstream keeps what fits and a marker.
    #[test]
    fn decode_captured_request_body_for_log_with_limit_truncates_zstd_expansion() {
        let compressed = zstd(&[b'x'; 1024]);
        let decoded = decode_request_body(&compressed, "zstd", 64);
        assert!(decoded.len() <= 128, "{}", decoded.len());
        assert_eq!(
            &*decoded,
            b"[ENCODED REQUEST BODY OMITTED: it decodes to over 64 bytes]"
        );
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&[b'x'; 1024]).unwrap();
        assert_eq!(
            &*decode_request_body(&gzip.finish().unwrap(), "gzip", 64),
            b"[ENCODED REQUEST BODY OMITTED: it decodes to over 64 bytes]"
        );
    }

    // Not upstream's: whether an answer is a stream.
    #[test]
    fn detects_streams() {
        assert!(is_streaming(Some("text/event-stream; charset=utf-8"), b""));
        assert!(!is_streaming(
            Some("application/json"),
            b"{\"stream\":true}"
        ));
        assert!(is_streaming(None, b"{\"stream\": true}"));
        assert!(is_streaming(Some(" "), b"{\"stream\":true}"));
        assert!(!is_streaming(None, b"{\"stream\":false}"));
    }
}
