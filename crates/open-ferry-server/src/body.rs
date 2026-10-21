// Ported from CLIProxyAPI sdk/api/handlers/request_body.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Request bodies.
//!
//! Deviations from upstream:
//! - A non-positive configured limit keeps the 64 MiB default; upstream
//!   disables its limit. Both raw and decoded bodies are bounded.
//! - `gzip` is decoded as well as `zstd`.
//! - The text of a zstd decoding error differs.
//! - A JSON body with 128 or more arrays and objects inside one another is
//!   refused with a 400 ([`check_depth`]), in the error format of the route
//!   that read it, before a translator or an executor sees it. Upstream
//!   forwards a body of any depth. Everything here that parses JSON reads at
//!   most [`MAX_DEPTH`] levels, and takes a deeper body for an empty one, so
//!   the model, the messages and the tools would be dropped without a word.

use std::io::Read;

use axum::body::Body;
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http::{HeaderMap, header};
use open_ferry_translate::go;
use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

use crate::errors::{ErrorMessage, invalid_request};

/// The error text for a body over the limit, as Go's `http.MaxBytesReader`
/// words it.
const TOO_LARGE: &str = "http: request body too large";

/// How many arrays and objects a JSON body may have inside one another. This
/// is as many as serde_json reads: it fails on the 128th, and the handlers
/// and translators read bodies with it.
pub(crate) const MAX_DEPTH: usize = 127;

/// Refuses a JSON `body` nested deeper than [`MAX_DEPTH`], with a 400 for
/// the route to answer in its own format. Every array and object counts,
/// empty or not. A body that isn't JSON passes, for the route to deal with as
/// it does now, and so does one that is no deeper than the limit. The body's
/// nesting is tracked on the heap, so a hostile one can't overflow the stack.
pub(crate) fn check_depth(body: &[u8]) -> Result<(), ErrorMessage> {
    if go::gjson_valid_within(body, MAX_DEPTH) || !go::gjson_valid(body) {
        return Ok(());
    }
    Err(ErrorMessage::new(
        400,
        format!(
            "Invalid request: the body is nested more than {MAX_DEPTH} levels deep \
             (arrays and objects inside one another)"
        ),
    ))
}

/// Reads a body of at most `limit` bytes as it came (upstream's
/// `GetRawData`), or answers 413 or 400.
pub(crate) async fn read_raw(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
) -> Result<Bytes, Response> {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > limit as u64) {
        return Err(invalid_request(413, TOO_LARGE));
    }
    let mut stream = body.into_data_stream();
    // The outer ingress layer produces one budget-owned chunk. Preserve its
    // owner through handlers and logging instead of copying and losing it.
    let mut first = None;
    let mut buffer = BytesMut::new();
    while let Some(frame) = stream.next().await {
        let frame = frame.map_err(|err| {
            let mut source: Option<&dyn std::error::Error> = Some(&err);
            while let Some(error) = source {
                if error.is::<crate::ingress::BodyTooLarge>() { return invalid_request(413, TOO_LARGE); }
                source = error.source();
            }
            invalid_request(400, &err.to_string())
        })?;
        let len = first.as_ref().map_or(buffer.len(), Bytes::len);
        if frame.len() > limit.saturating_sub(len) {
            return Err(invalid_request(413, TOO_LARGE));
        }
        if first.is_none() && buffer.is_empty() {
            first = Some(frame);
            continue;
        }
        if let Some(first) = first.take() {
            buffer.extend_from_slice(&first);
        }
        buffer.extend_from_slice(&frame);
    }
    Ok(first.unwrap_or_else(|| buffer.freeze()))
}

/// Reads a body and decodes its `Content-Encoding` (upstream's
/// `ReadRequestBody`), or answers 413 or 400. A body that fails to decode is
/// used as it came when it is JSON.
pub(crate) async fn read_decoded(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
) -> Result<Bytes, Response> {
    let raw = read_raw(headers, body, limit).await?;
    let encoding = headers
        .get(header::CONTENT_ENCODING)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
        .unwrap_or_default();
    if encoding.is_empty() || encoding.eq_ignore_ascii_case("identity") {
        return Ok(raw);
    }
    match decode(&raw, &encoding, limit) {
        Ok(decoded) => {
            crate::ingress::retain_decoded(&decoded);
            Ok(decoded)
        }
        Err(DecodeError::TooLarge) => Err(invalid_request(413, TOO_LARGE)),
        Err(DecodeError::Budget) => {
            crate::ingress::warn_budget();
            Err(crate::ingress::rejected(
                503,
                "request body buffer budget exhausted",
            ))
        }
        Err(DecodeError::Failed(_)) if go::json_valid(&raw) => Ok(raw),
        Err(DecodeError::Failed(err)) => Err(invalid_request(400, &err)),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DecodeError {
    /// The server-wide decoded storage budget was exhausted.
    Budget,
    /// The decoded body is over the limit.
    TooLarge,
    /// The body couldn't be decoded; the text says why.
    Failed(String),
}

/// Undoes each encoding in `encoding`, last first.
fn decode(raw: &Bytes, encoding: &str, limit: usize) -> Result<Bytes, DecodeError> {
    let mut body = raw.clone();
    for part in encoding.split(',').rev() {
        let name = go::to_lower(part.trim());
        body = match name.as_str() {
            "" | "identity" => continue,
            "zstd" => decode_zstd(&body, limit)?,
            "gzip" | "x-gzip" => decode_gzip(&body, limit)?,
            _ => {
                return Err(DecodeError::Failed(format!(
                    "unsupported request content encoding: {name}"
                )));
            }
        };
    }
    Ok(body)
}

/// Decodes a zstd stream of any number of frames, skipping skippable ones.
fn decode_zstd(mut input: &[u8], limit: usize) -> Result<Bytes, DecodeError> {
    let failed = |err: &dyn std::fmt::Display| {
        DecodeError::Failed(format!("failed to decode zstd request body: {err}"))
    };
    let mut decoder = FrameDecoder::new();
    let window = limit.min(8 << 20);
    decoder.set_max_window_size(window as u64);
    // Charge before init or expansion. This covers both ring-buffer allocations
    // during growth, one block's literals/sequences and entropy tables. Decoding
    // one block at a time and draining in fixed stack chunks bounds that scratch.
    let working_bytes = window.saturating_mul(4)
        .saturating_add(window.min(128 << 10).saturating_mul(20))
        .saturating_add(64 << 10);
    let _working = crate::ingress::reserve_decoder(working_bytes).map_err(|()| DecodeError::Budget)?;
    let mut output = crate::ingress::BudgetBuffer::new(crate::ingress::request_budget());
    while !input.is_empty() {
        match decoder.init(&mut input) {
            Ok(()) => {}
            Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                length,
                ..
            })) => {
                input = input
                    .get(length as usize..)
                    .ok_or_else(|| failed(&"truncated skippable frame"))?;
                continue;
            }
            Err(err) => return Err(failed(&err)),
        }
        loop {
            decoder
                .decode_blocks(&mut input, BlockDecodingStrategy::UptoBlocks(1))
                .map_err(|err| failed(&err))?;
            while decoder.can_collect() != 0 {
                if decoder.can_collect() > limit.saturating_sub(output.len()) {
                    return Err(DecodeError::TooLarge);
                }
                let mut chunk = [0; 16 << 10];
                let read = decoder.read(&mut chunk).map_err(|err| failed(&err))?;
                output.push(&chunk[..read]).map_err(|()| DecodeError::Budget)?;
            }
            if decoder.is_finished() {
                break;
            }
        }
        if let Some(expected) = decoder.get_checksum_from_data()
            && decoder.get_calculated_checksum() != Some(expected)
        {
            return Err(failed(&"checksum mismatch"));
        }
    }
    Ok(output.finish())
}

/// Decodes a gzip stream of any number of members.
fn decode_gzip(input: &[u8], limit: usize) -> Result<Bytes, DecodeError> {
    let mut output = crate::ingress::BudgetBuffer::new(crate::ingress::request_budget());
    let mut decoder = flate2::read::MultiGzDecoder::new(input);
    let mut chunk = [0; 16 * 1024];
    loop {
        let read = decoder.read(&mut chunk).map_err(|err| {
            DecodeError::Failed(format!("failed to decode gzip request body: {err}"))
        })?;
        if read == 0 {
            break;
        }
        if read > limit.saturating_sub(output.len()) {
            return Err(DecodeError::TooLarge);
        }
        output
            .push(&chunk[..read])
            .map_err(|()| DecodeError::Budget)?;
    }
    Ok(output.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_zstd_128_mib_window_is_rejected_before_allocating_it() {
        let frame = [0x28, 0xb5, 0x2f, 0xfd, 0, 0x88, 1, 0, 0];
        crate::allocation_probe::start();
        let result = decode_zstd(&frame, 64 << 20);
        let largest = crate::allocation_probe::finish();
        assert!(matches!(result, Err(DecodeError::Failed(_))));
        assert!(largest < 1 << 20, "allocated {largest} bytes for a rejected window");
    }

    fn zstd_frame(payload: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(payload, ruzstd::encoding::CompressionLevel::Fastest)
    }

    fn gzip(payload: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    fn arrays(depth: usize, inner: &str) -> String {
        format!("{}{inner}{}", "[".repeat(depth), "]".repeat(depth))
    }

    fn objects(depth: usize, inner: &str) -> String {
        format!("{}{inner}{}", r#"{"a":"#.repeat(depth), "}".repeat(depth))
    }

    // Not upstream's: the limit is the deepest body serde_json reads, so a
    // body that gets by is never read as an empty one further on.
    #[test]
    fn the_depth_limit_is_what_serde_json_reads() {
        let reads = |text: &str| serde_json::from_str::<serde_json::Value>(text).is_ok();
        assert!(reads(&arrays(MAX_DEPTH, "0")));
        assert!(!reads(&arrays(MAX_DEPTH + 1, "0")));
        assert!(reads(&objects(MAX_DEPTH, "0")));
        assert!(!reads(&objects(MAX_DEPTH + 1, "0")));
        // That is what the handlers do to a body they can't read.
        assert_eq!(
            crate::handlers::parse_body(arrays(MAX_DEPTH + 1, "0").as_bytes()),
            serde_json::Value::Null
        );
    }

    // Not upstream's: upstream forwards a body of any depth.
    #[test]
    fn refuses_bodies_nested_deeper_than_the_limit() {
        let refused = |text: &str| check_depth(text.as_bytes()).unwrap_err();
        let passes = |text: &str| assert!(check_depth(text.as_bytes()).is_ok(), "{text}");
        passes(&arrays(MAX_DEPTH, "0"));
        passes(&objects(MAX_DEPTH, "0"));
        for text in [
            arrays(MAX_DEPTH + 1, "0"),
            arrays(MAX_DEPTH + 2, "0"),
            objects(MAX_DEPTH + 1, "0"),
            objects(MAX_DEPTH + 1, r#"{"b":[1]}"#),
        ] {
            let error = refused(&text);
            assert_eq!(error.status, 400, "{error:?}");
            assert!(error.text.contains("127"), "{}", error.text);
        }
        // An empty array or object is a level, and a mixture counts alike.
        passes(&arrays(MAX_DEPTH - 1, "[]"));
        passes(&arrays(MAX_DEPTH - 1, "{}"));
        refused(&arrays(MAX_DEPTH, "[]"));
        refused(&arrays(MAX_DEPTH, "{}"));
        refused(&format!(
            "{}0{}",
            r#"[{"a":"#.repeat(MAX_DEPTH / 2 + 1),
            "}]".repeat(MAX_DEPTH / 2 + 1)
        ));
        // White space around the body doesn't change what it is.
        refused(&format!(" \r\n{}\t", arrays(MAX_DEPTH + 1, "0")));
        // A body with no depth to speak of, or not JSON at all, is left to
        // the route.
        for body in ["", "nope", "0", r#""[[[""#, r#"{"a":[1,{"b":null}]}"#] {
            passes(body);
        }
        // So is one that isn't valid JSON, however deep it starts.
        let broken = arrays(MAX_DEPTH + 10, "0");
        passes(&broken[1..]);
        passes(&format!("{broken} 1"));
        // It reads a body nested a million deep without overflowing the stack.
        refused(&arrays(1_000_000, "0"));
    }

    #[test]
    fn decodes_zstd_frames_and_skips_skippable_ones() {
        let mut input = zstd_frame(b"{\"a\":");
        // A skippable frame: magic 0x184D2A50, length 3.
        input.extend_from_slice(&[0x50, 0x2A, 0x4D, 0x18, 3, 0, 0, 0, 1, 2, 3]);
        input.extend_from_slice(&zstd_frame(b"1}"));
        let decoded = decode(&Bytes::from(input), "zstd", 1 << 20).unwrap();
        assert_eq!(&decoded[..], b"{\"a\":1}");
    }

    #[test]
    fn decodes_layers_last_first() {
        let input = gzip(&zstd_frame(b"{}"));
        let decoded = decode(&Bytes::from(input), " zstd , identity,, GZIP", 1 << 20).unwrap();
        assert_eq!(&decoded[..], b"{}");
    }

    #[test]
    fn reports_bad_encodings() {
        assert_eq!(
            decode(&Bytes::from_static(b"x"), "br", 10),
            Err(DecodeError::Failed(
                "unsupported request content encoding: br".into()
            ))
        );
        assert!(matches!(
            decode(&Bytes::from_static(b"not zstd"), "zstd", 10),
            Err(DecodeError::Failed(text)) if text.starts_with("failed to decode zstd request body: ")
        ));
        let big = vec![b'a'; 4096];
        assert_eq!(
            decode(&Bytes::from(zstd_frame(&big)), "zstd", 4095),
            Err(DecodeError::TooLarge)
        );
        assert_eq!(
            decode(&Bytes::from(gzip(&big)), "gzip", 4095),
            Err(DecodeError::TooLarge)
        );
    }

    #[tokio::test]
    async fn falls_back_to_raw_json_and_limits_size() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, "zstd".parse().unwrap());
        let read = |body: &'static [u8], headers: HeaderMap, limit| async move {
            read_decoded(&headers, Body::from(body), limit).await
        };
        assert_eq!(
            &read(b"{\"plain\":true}", headers.clone(), 100)
                .await
                .unwrap()[..],
            b"{\"plain\":true}"
        );
        assert_eq!(
            read(b"nope", headers.clone(), 100)
                .await
                .unwrap_err()
                .status(),
            400
        );
        assert_eq!(
            read(b"0123456789", HeaderMap::new(), 9)
                .await
                .unwrap_err()
                .status(),
            413
        );
        assert_eq!(
            &read(b"0123456789", HeaderMap::new(), 10).await.unwrap()[..],
            b"0123456789"
        );
        let mut declared = HeaderMap::new();
        declared.insert(header::CONTENT_LENGTH, "11".parse().unwrap());
        assert_eq!(read(b"", declared, 10).await.unwrap_err().status(), 413);
    }

    // TestServerMaxRequestBodyBytesRejectsContentLengthAndChunkedBodies:
    // a MaxInt64 configured cap must not overflow the decoded reader.
    #[tokio::test]
    async fn the_largest_configured_limit_accepts_small_decoded_bodies() {
        for (encoding, input) in [("zstd", zstd_frame(b"{}")), ("gzip", gzip(b"{}"))] {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_ENCODING, encoding.parse().unwrap());
            assert_eq!(
                read_decoded(&headers, Body::from(input), i64::MAX as usize)
                    .await
                    .unwrap(),
                "{}"
            );
        }
    }
}
