// Ported from CLIProxyAPI internal/runtime/executor/openai_compat_executor.go
// (prepareOpenAICompatImagesPayload, cloneOpenAICompatMIMEHeader,
// rewriteOpenAICompatImagesMultipartPayload and the stream forwarding of
// executeImagesStream) and internal/runtime/executor/codex_openai_images.go
// (the stream forwarding of executeDirectOpenAIImageStream) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What the executors that call an Image API (`/images/generations` and
//! `/images/edits`) share: the body they send, with the model and `stream`
//! set ([`prepare_payload`]), and the stream they pass back as it comes
//! ([`raw_stream`]).
//!
//! A JSON body gets the model and, for a stream, `"stream": true`; a body
//! that doesn't stream loses `stream`. A `multipart/form-data` body is read
//! and written again, with the `model` and `stream` fields first, then the
//! other fields and the files, each file with its own header and
//! `application/octet-stream` when it has no type. Any other body goes as
//! it came. Bodies stay in memory, and nothing here logs them: they hold
//! prompts and images.
//!
//! Deviations from upstream:
//! - A JSON body that changes is written again whole and compact, where
//!   sjson edits the client's bytes in place; it means the same. A body
//!   that isn't an object, or that Go reads as JSON but serde doesn't (a
//!   string with invalid UTF-8), goes as it came; sjson would edit it or
//!   fail.
//! - A form's fields and files are written in name order; Go's maps give
//!   them in random order.
//! - A stream is passed on a line at a time, where upstream passes on what
//!   each 32 KiB read gives, so that a secret the request sent is redacted
//!   from it (see `Policy::Client` in the crate's `redact` module). A line
//!   longer than 50 MiB is passed on in pieces. When the connection fails,
//!   the error is redacted too.

use std::borrow::Cow;

use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::exec::{ChunkStream, ErrorKind, ExecError};
use open_ferry_core::multipart::{
    Form, MAX_FORM_MEMORY, Reader, Writer, file_content_disposition, parse_media_type,
};
use open_ferry_translate::go::{json_valid, trim_space};
use open_ferry_translate::json::exact;
use serde_json::Value;

use crate::codex::client::error_chain;
use crate::json::{delete, get, set};
use crate::observe_send::{self, BodyTap};
use crate::redact::{Policy, Secrets};

/// The most of a stream held back waiting for the end of a line.
const MAX_HELD: usize = 52_428_800;

/// The body to send for `payload`, a client's image request sent with
/// `content_type`, and its content type: with `model`, when it isn't empty,
/// and `stream` (`prepareOpenAICompatImagesPayload`).
pub(crate) fn prepare_payload(
    payload: &Bytes,
    model: &str,
    content_type: &str,
    stream: bool,
) -> Result<(Bytes, String), ExecError> {
    let model = model.trim();
    let content_type = content_type.trim();
    if json_valid(payload) {
        return Ok((
            edit_json(payload, model, stream),
            "application/json".to_owned(),
        ));
    }
    let Some(boundary) = multipart_boundary(content_type) else {
        return Ok((payload.clone(), content_type.to_owned()));
    };
    if boundary.is_empty() {
        return Err(ExecError::new(
            ErrorKind::Upstream,
            "multipart boundary is missing",
        ));
    }
    rewrite_form(payload, model, &boundary, stream)
}

/// The trimmed `boundary` of `content_type`, if it names a `multipart/`
/// type; empty when the type has none.
pub(crate) fn multipart_boundary(content_type: &str) -> Option<Vec<u8>> {
    let (kind, params) = parse_media_type(content_type.trim().as_bytes()).ok()?;
    if !kind.trim().starts_with("multipart/") {
        return None;
    }
    let boundary = params
        .get("boundary")
        .map(Vec::as_slice)
        .unwrap_or_default();
    Some(trim_space(boundary).to_vec())
}

/// Reads `payload`, a form with `boundary`, whole (Go's `ReadForm` with
/// 32 MiB of memory, though nothing goes to disk here).
pub(crate) fn read_form(payload: &Bytes, boundary: &[u8]) -> Result<Form, String> {
    Reader::new(payload.clone(), boundary)
        .read_form(MAX_FORM_MEMORY)
        .map_err(|error| error.to_string())
}

/// A JSON body with `model` and `stream` set, written again only if that
/// changed it.
fn edit_json(payload: &Bytes, model: &str, stream: bool) -> Bytes {
    let Ok(mut body @ Value::Object(_)) = exact::from_slice(payload) else {
        return payload.clone();
    };
    let mut changed = false;
    if !model.is_empty()
        && !matches!(get(&body, "model"), Some(Value::String(current)) if current == model)
    {
        changed |= set(&mut body, "model", Value::from(model));
    }
    if stream {
        if get(&body, "stream") != Some(&Value::Bool(true)) {
            changed |= set(&mut body, "stream", Value::Bool(true));
        }
    } else {
        changed |= delete(&mut body, "stream");
    }
    if changed {
        Bytes::from(body.to_string())
    } else {
        payload.clone()
    }
}

/// A form written again with `model` and `stream` first
/// (`rewriteOpenAICompatImagesMultipartPayload`).
fn rewrite_form(
    payload: &Bytes,
    model: &str,
    boundary: &[u8],
    stream: bool,
) -> Result<(Bytes, String), ExecError> {
    let form = read_form(payload, boundary).map_err(|error| {
        ExecError::new(
            ErrorKind::Upstream,
            format!("read multipart form failed: {error}"),
        )
    })?;
    let mut writer = Writer::new();
    if !model.is_empty() {
        writer.write_field("model", model.as_bytes());
    }
    if stream {
        writer.write_field("stream", b"true");
    }
    for (name, values) in form.values() {
        if name == "model" || name == "stream" {
            continue;
        }
        for value in values {
            writer.write_field(name, value);
        }
    }
    for (name, files) in form.files() {
        for file in files {
            let mut header = file.header.clone();
            header.set(
                "Content-Disposition",
                file_content_disposition(name, &file.filename),
            );
            if header.get_str("Content-Type").is_empty() {
                header.set("Content-Type", "application/octet-stream");
            }
            writer.write_part(&header, &file.data);
        }
    }
    let content_type = writer.form_data_content_type();
    Ok((Bytes::from(writer.finish()), content_type))
}

/// Passes `response`'s body on as it comes, a line at a time, each with
/// `secrets` redacted. A connection that fails ends it with the error,
/// after what was read of the last line.
pub(crate) fn raw_stream(response: reqwest::Response, secrets: Secrets) -> ChunkStream {
    let state = Raw {
        tap: BodyTap::of(&response),
        response,
        held: Vec::new(),
        secrets,
        failure: None,
        done: false,
    };
    futures_util::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(error) = state.failure.take() {
                return Some((Err(error), state));
            }
            if state.done {
                return None;
            }
            match state.response.chunk().await {
                Ok(Some(chunk)) => {
                    if let Some(tap) = &state.tap {
                        tap.chunk(&chunk);
                    }
                    if let Some(piece) = state.take_lines(&chunk) {
                        return Some((Ok(piece), state));
                    }
                }
                Ok(None) => {
                    state.done = true;
                    let rest = std::mem::take(&mut state.held);
                    if let Some(piece) = state.redact(rest) {
                        return Some((Ok(piece), state));
                    }
                }
                Err(error) => {
                    state.done = true;
                    let error = ExecError::new(
                        ErrorKind::Upstream,
                        state
                            .secrets
                            .text(error_chain(&error.without_url()), Policy::Client),
                    );
                    observe_send::attempt_error(state.tap.as_ref(), &error);
                    state.failure = Some(error);
                    let rest = std::mem::take(&mut state.held);
                    if let Some(piece) = state.redact(rest) {
                        return Some((Ok(piece), state));
                    }
                }
            }
        }
    })
    .boxed()
}

/// Where [`raw_stream`] is.
struct Raw {
    response: reqwest::Response,
    tap: Option<BodyTap>,
    /// What was read of a line not yet passed on.
    held: Vec<u8>,
    secrets: Secrets,
    failure: Option<ExecError>,
    done: bool,
}

impl Raw {
    /// The lines `chunk` completes, redacted, holding back the rest, or
    /// what is held when it has grown to [`MAX_HELD`].
    fn take_lines(&mut self, chunk: &[u8]) -> Option<Bytes> {
        match chunk.iter().rposition(|&b| b == b'\n') {
            Some(end) => {
                let (lines, rest) = chunk.split_at(end + 1);
                let mut piece = std::mem::take(&mut self.held);
                piece.extend_from_slice(lines);
                self.held.extend_from_slice(rest);
                self.redact(piece)
            }
            None => {
                self.held.extend_from_slice(chunk);
                if self.held.len() < MAX_HELD {
                    return None;
                }
                let piece = std::mem::take(&mut self.held);
                self.redact(piece)
            }
        }
    }

    /// `piece` with the secrets redacted, unless it is empty.
    fn redact(&self, piece: Vec<u8>) -> Option<Bytes> {
        if piece.is_empty() {
            return None;
        }
        Some(match self.secrets.bytes(&piece, Policy::Client) {
            Cow::Owned(redacted) => Bytes::from(redacted),
            Cow::Borrowed(_) => Bytes::from(piece),
        })
    }
}

#[cfg(test)]
mod tests {
    use futures_util::stream;
    use open_ferry_core::multipart::Header;

    use super::*;

    type Part<'a> = (&'a str, Option<&'a str>, Option<&'a str>, &'a [u8]);

    fn form(parts: &[Part<'_>]) -> (Bytes, String) {
        let mut writer = Writer::new();
        for (name, filename, content_type, data) in parts {
            match filename {
                Some(filename) => {
                    let mut header = Header::new();
                    header.set(
                        "Content-Disposition",
                        file_content_disposition(name, filename),
                    );
                    if let Some(content_type) = content_type {
                        header.set("Content-Type", *content_type);
                    }
                    writer.write_part(&header, data);
                }
                None => writer.write_field(name, data),
            }
        }
        let content_type = writer.form_data_content_type();
        (Bytes::from(writer.finish()), content_type)
    }

    fn read(body: &Bytes, content_type: &str) -> Form {
        let boundary = multipart_boundary(content_type).unwrap();
        read_form(body, &boundary).unwrap()
    }

    // Not upstream's: a JSON body gets the model, and `stream` only when it
    // streams; a body already right goes as it came.
    #[test]
    fn json_bodies_get_the_model_and_stream() {
        let payload =
            Bytes::from_static(br#"{"model":"alias","prompt":"p","n":1.50,"stream":true}"#);
        let (body, content_type) = prepare_payload(&payload, " gpt-image-2 ", "", false).unwrap();
        assert_eq!(content_type, "application/json");
        assert_eq!(
            &body[..],
            br#"{"model":"gpt-image-2","prompt":"p","n":1.50}"#
        );

        let (body, _) = prepare_payload(&payload, "gpt-image-2", "text/plain", true).unwrap();
        assert_eq!(
            &body[..],
            br#"{"model":"gpt-image-2","prompt":"p","n":1.50,"stream":true}"#
        );

        let same = Bytes::from_static(b"{ \"model\" : \"m\", \"stream\" : true }");
        let (body, _) = prepare_payload(&same, "m", "", true).unwrap();
        assert_eq!(body, same);
        let (body, _) = prepare_payload(&same, "", "", true).unwrap();
        assert_eq!(body, same);

        let list = Bytes::from_static(b"[1]");
        assert_eq!(prepare_payload(&list, "m", "", false).unwrap().0, list);
    }

    // Not upstream's: a form is written again with the model and `stream`
    // first, its other fields, and its files with their own headers.
    #[test]
    fn forms_are_written_again_with_the_model_first() {
        let (payload, content_type) = form(&[
            ("prompt", None, None, b"a cat"),
            ("model", None, None, b"alias"),
            ("stream", None, None, b"false"),
            ("image[]", Some("a.png"), Some("image/png"), b"png-data"),
            ("mask", Some("m.bin"), None, b"mask-data"),
        ]);
        let (body, sent_type) =
            prepare_payload(&payload, "gpt-image-2", &format!(" {content_type} "), true).unwrap();
        assert!(sent_type.starts_with("multipart/form-data; boundary="));
        assert_ne!(sent_type, content_type);
        let text = String::from_utf8_lossy(&body);
        let model = text.find("name=\"model\"").unwrap();
        let stream = text.find("name=\"stream\"").unwrap();
        let prompt = text.find("name=\"prompt\"").unwrap();
        assert!(model < stream && stream < prompt);

        let sent = read(&body, &sent_type);
        assert_eq!(&sent.value("model").unwrap()[..], b"gpt-image-2");
        assert_eq!(&sent.value("stream").unwrap()[..], b"true");
        assert_eq!(&sent.value("prompt").unwrap()[..], b"a cat");
        let image = &sent.files_of("image[]")[0];
        assert_eq!(image.filename, "a.png");
        assert_eq!(image.header.get_str("Content-Type"), "image/png");
        assert_eq!(&image.data[..], b"png-data");
        let mask = &sent.files_of("mask")[0];
        assert_eq!(
            mask.header.get_str("Content-Type"),
            "application/octet-stream"
        );
        assert_eq!(&mask.data[..], b"mask-data");

        // Without a model or a stream, neither field is written.
        let (body, sent_type) = prepare_payload(&payload, "", &content_type, false).unwrap();
        let sent = read(&body, &sent_type);
        assert!(sent.value("model").is_none());
        assert!(sent.value("stream").is_none());
    }

    // Not upstream's: a form without a boundary fails, one that doesn't read
    // fails with Go's text, and a body that is neither JSON nor a form goes
    // as it came, with its trimmed content type.
    #[test]
    fn other_bodies() {
        let payload = Bytes::from_static(b"not json");
        let error = prepare_payload(&payload, "m", "multipart/form-data", false).unwrap_err();
        assert_eq!(error.to_string(), "multipart boundary is missing");
        let error =
            prepare_payload(&payload, "m", "multipart/form-data; boundary=x", false).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("read multipart form failed: ")
        );
        let (body, content_type) = prepare_payload(&payload, "m", " text/plain ", false).unwrap();
        assert_eq!(body, payload);
        assert_eq!(content_type, "text/plain");
        let (_, content_type) = prepare_payload(&payload, "m", "multipart", false).unwrap();
        assert_eq!(content_type, "multipart");
    }

    fn response(chunks: Vec<&'static [u8]>, fail: bool) -> reqwest::Response {
        let mut items: Vec<Result<Bytes, std::io::Error>> = chunks
            .into_iter()
            .map(|chunk| Ok(Bytes::from_static(chunk)))
            .collect();
        if fail {
            items.push(Err(std::io::Error::other("connection reset")));
        }
        let body = reqwest::Body::wrap_stream(stream::iter(items));
        reqwest::Response::from(http::Response::new(body))
    }

    async fn drain(stream: ChunkStream) -> (Vec<Bytes>, Option<String>) {
        let results: Vec<_> = stream.collect().await;
        let mut pieces = Vec::new();
        let mut error = None;
        for result in results {
            match result {
                Ok(piece) => pieces.push(piece),
                Err(failure) => error = Some(failure.to_string()),
            }
        }
        (pieces, error)
    }

    // Not upstream's: a stream passes on whole lines, so a secret split
    // across reads is still redacted, and the rest comes at the end.
    #[tokio::test]
    async fn raw_streams_pass_on_whole_lines_redacted() {
        let mut secrets = Secrets::default();
        secrets.add("sk-secret-token");
        let chunks: Vec<&'static [u8]> = vec![
            b"event: a\ndata: {\"k\":\"sk-sec",
            b"ret-token\"}\n\n",
            b"tail",
        ];
        let (pieces, error) = drain(raw_stream(response(chunks, false), secrets)).await;
        assert!(error.is_none());
        let pieces: Vec<&[u8]> = pieces.iter().map(|piece| &piece[..]).collect();
        assert_eq!(
            pieces,
            [
                b"event: a\n".as_slice(),
                b"data: {\"k\":\"[redacted]\"}\n\n".as_slice(),
                b"tail".as_slice()
            ]
        );
    }

    // Not upstream's: a failed connection gives what was read, then the
    // error.
    #[tokio::test]
    async fn raw_streams_end_with_the_error() {
        let (pieces, error) = drain(raw_stream(
            response(vec![b"data: x\npart"], true),
            Secrets::default(),
        ))
        .await;
        let pieces: Vec<&[u8]> = pieces.iter().map(|piece| &piece[..]).collect();
        assert_eq!(pieces, [b"data: x\n".as_slice(), b"part".as_slice()]);
        assert!(error.is_some());
    }
}
