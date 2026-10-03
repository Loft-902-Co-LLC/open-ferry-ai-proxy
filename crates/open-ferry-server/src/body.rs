// Ported from CLIProxyAPI sdk/api/handlers/request_body.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Request bodies.
//!
//! Deviations from upstream:
//! - A body over the configured limit, before or after decoding, gets a 413.
//!   Upstream reads bodies of any size.
//! - `gzip` is decoded as well as `zstd`.
//! - The text of a zstd decoding error differs.

use std::io::Read;

use axum::body::Body;
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http::{HeaderMap, header};
use open_ferry_translate::go;
use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

use crate::errors::invalid_request;

/// The error text for a body over the limit, as Go's `http.MaxBytesReader`
/// words it.
const TOO_LARGE: &str = "http: request body too large";

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
    let mut buffer = BytesMut::new();
    while let Some(frame) = stream.next().await {
        let frame = frame.map_err(|err| invalid_request(400, &err.to_string()))?;
        if buffer.len() + frame.len() > limit {
            return Err(invalid_request(413, TOO_LARGE));
        }
        buffer.extend_from_slice(&frame);
    }
    Ok(buffer.freeze())
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
        Ok(decoded) => Ok(decoded),
        Err(DecodeError::TooLarge) => Err(invalid_request(413, TOO_LARGE)),
        Err(DecodeError::Failed(_)) if go::json_valid(&raw) => Ok(raw),
        Err(DecodeError::Failed(err)) => Err(invalid_request(400, &err)),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DecodeError {
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
            "zstd" => Bytes::from(decode_zstd(&body, limit)?),
            "gzip" | "x-gzip" => Bytes::from(decode_gzip(&body, limit)?),
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
fn decode_zstd(mut input: &[u8], limit: usize) -> Result<Vec<u8>, DecodeError> {
    let failed = |err: &dyn std::fmt::Display| {
        DecodeError::Failed(format!("failed to decode zstd request body: {err}"))
    };
    let mut decoder = FrameDecoder::new();
    let mut output = Vec::new();
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
                .decode_blocks(&mut input, BlockDecodingStrategy::UptoBytes(1 << 20))
                .map_err(|err| failed(&err))?;
            if let Some(chunk) = decoder.collect() {
                if output.len() + chunk.len() > limit {
                    return Err(DecodeError::TooLarge);
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
            return Err(failed(&"checksum mismatch"));
        }
    }
    Ok(output)
}

/// Decodes a gzip stream of any number of members.
fn decode_gzip(input: &[u8], limit: usize) -> Result<Vec<u8>, DecodeError> {
    let mut output = Vec::new();
    flate2::read::MultiGzDecoder::new(input)
        .take(limit as u64 + 1)
        .read_to_end(&mut output)
        .map_err(|err| DecodeError::Failed(format!("failed to decode gzip request body: {err}")))?;
    if output.len() > limit {
        return Err(DecodeError::TooLarge);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zstd_frame(payload: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(payload, ruzstd::encoding::CompressionLevel::Fastest)
    }

    fn gzip(payload: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
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
}
