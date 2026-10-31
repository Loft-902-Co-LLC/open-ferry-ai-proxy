//! Negotiated response compression with commit-time decisions and per-frame flushing.
use crate::AppState;
use axum::{
    body::Body,
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, header};
use http_body::{Body as HttpBody, Frame};
use http_body_util::BodyExt;
use std::{
    collections::HashMap,
    io::{self, Write},
    pin::Pin,
    task::{Context, Poll},
};

const CHUNK: usize = 16 * 1024;
const ZSTD_WINDOW_LOG: u32 = 20;

pub(crate) async fn layer(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let settings = state.settings();
    let config = &settings.config.response_compression;
    if !config.is_enabled()
        || request.headers().get(header::UPGRADE).is_some_and(|v| {
            v.to_str()
                .is_ok_and(|v| v.trim().eq_ignore_ascii_case("websocket"))
        })
    {
        return next.run(request).await;
    }
    let accept = request
        .headers()
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()))
        .collect::<Vec<_>>()
        .join(",");
    let (encoding, identity) = negotiate(&accept);
    let head = request.method() == Method::HEAD;
    let min = if identity {
        usize::try_from(config.effective_min_bytes()).unwrap_or(usize::MAX)
    } else {
        0
    };
    let response = next.run(request).await;
    compress(response, if head { None } else { encoding }, min).await
}

fn negotiate(header: &str) -> (Option<&'static str>, bool) {
    let mut qualities = HashMap::new();
    for token in header.split(',') {
        let mut parts = token.split(';');
        let name = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
        let mut q = 1.0;
        for param in parts {
            if let Some((key, value)) = param.split_once('=')
                && key.trim().eq_ignore_ascii_case("q")
            {
                q = open_ferry_translate::go::parse_float_checked(value.trim())
                    .filter(|q| (0.0..=1.0).contains(q))
                    .unwrap_or(0.0);
            }
        }
        qualities.insert(name, q);
    }
    let wildcard = qualities.get("*").copied();
    let identity = qualities
        .get("identity")
        .copied()
        .map_or(wildcard != Some(0.0), |q| q > 0.0);
    let mut selected = None;
    let mut best = 0.0;
    for codec in ["zstd", "gzip"] {
        let q = qualities.get(codec).copied().or(wildcard).unwrap_or(0.0);
        if q > best {
            selected = Some(codec);
            best = q;
        }
    }
    (selected, identity)
}

fn vary(headers: &mut HeaderMap) {
    let mut tokens: Vec<String> = Vec::new();
    for value in headers.get_all(header::VARY) {
        for token in value
            .to_str()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !tokens.iter().any(|v| v.eq_ignore_ascii_case(token)) {
                tokens.push(token.to_owned());
            }
        }
    }
    if !tokens
        .iter()
        .any(|v| v.eq_ignore_ascii_case("accept-encoding"))
    {
        tokens.push("Accept-Encoding".into());
    }
    if let Ok(value) = HeaderValue::from_str(&tokens.join(", ")) {
        headers.insert(header::VARY, value);
    }
}

fn nonempty(headers: &HeaderMap, name: header::HeaderName) -> bool {
    headers
        .get(name)
        .is_some_and(|value| !value.as_bytes().is_empty())
}

/// Each frame of these responses is an upstream writer flush. In particular,
/// non-SSE streams must decide on their first frame, without threshold staging.
#[derive(Clone, Copy)]
pub(crate) struct FlushFrames;

type Frames = Vec<Result<Frame<Bytes>, axum::Error>>;

fn prepend(frames: Frames, body: Body) -> Body {
    if frames.is_empty() {
        return body;
    }
    Body::new(http_body_util::StreamBody::new(
        futures_util::stream::iter(frames).chain(body.into_stream()),
    ))
}

async fn compress(response: Response, encoding: Option<&'static str>, min: usize) -> Response {
    let (mut parts, mut body) = response.into_parts();
    vary(&mut parts.headers);
    let fixed = body.size_hint().exact().is_some();
    let flushed = parts.extensions.get::<FlushFrames>().is_some();
    let mut peeked = std::collections::VecDeque::new();
    if encoding.is_some()
        && !nonempty(&parts.headers, header::CONTENT_TYPE)
        && !nonempty(&parts.headers, header::CONTENT_ENCODING)
        && parts.status.as_u16() >= 200
        && !matches!(parts.status.as_u16(), 204 | 206 | 304)
        && let Some(first) = body.frame().await
    {
        if let Ok(frame) = &first
            && let Some(data) = frame.data_ref()
            && !data.is_empty()
        {
            parts.headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(open_ferry_core::multipart::detect_content_type(data)),
            );
        }
        peeked.push_back(first);
    }
    let media = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let compressible = media.starts_with("text/")
        || media == "application/json"
        || (media.starts_with("application/") && media.ends_with("+json"))
        || media == "application/javascript"
        || media == "image/svg+xml";
    let streaming = media == "text/event-stream";
    let candidate = encoding.filter(|_| {
        compressible
            && parts.status.as_u16() >= 200
            && !matches!(parts.status.as_u16(), 204 | 206 | 304)
            && !nonempty(&parts.headers, header::CONTENT_ENCODING)
    });
    let known_small = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|n| n < min);
    let selected = candidate.filter(|_| streaming || !known_small);
    if candidate.is_some() && fixed && !streaming && !flushed {
        return complete(
            parts,
            prepend(peeked.into_iter().collect(), body),
            selected,
            min,
        )
        .await;
    }
    if selected.is_none() {
        return Response::from_parts(parts, prepend(peeked.into_iter().collect(), body));
    }
    let mut staged = Vec::new();
    let mut buffered = bytes::BytesMut::new();
    let mut size = 0usize;
    let mut ended = false;
    // A flush forces a decision after this frame, even below min. Ordinary
    // buffered writers retain data only until the configured threshold or EOF.
    while !streaming && size < min {
        let next = match peeked.pop_front() {
            Some(frame) => Some(frame),
            None => body.frame().await,
        };
        match next {
            Some(Ok(frame)) => match frame.into_data() {
                Ok(data) => {
                    size = size.saturating_add(data.len());
                    if size < min && !flushed {
                        buffered.extend_from_slice(&data);
                    } else {
                        if !buffered.is_empty() {
                            staged.push(Ok(Frame::data(buffered.split().freeze())));
                        }
                        staged.push(Ok(Frame::data(data)));
                    }
                }
                Err(trailer) => {
                    if !buffered.is_empty() {
                        staged.push(Ok(Frame::data(buffered.split().freeze())));
                    }
                    staged.push(Ok(trailer));
                    ended = true;
                    break;
                }
            },
            Some(Err(error)) => {
                staged.push(Err(error));
                break;
            }
            None => {
                ended = true;
                break;
            }
        }
        if body.is_end_stream() && peeked.is_empty() {
            ended = true;
            break;
        }
        if flushed {
            break;
        }
    }
    if !buffered.is_empty() {
        staged.push(Ok(Frame::data(buffered.freeze())));
    }
    staged.extend(peeked);
    let source = prepend(staged, body);
    if ended && !flushed {
        return complete(parts, source, selected, min).await;
    }
    encoded(parts, source, selected.filter(|_| streaming || size >= min))
}

async fn complete(
    mut parts: http::response::Parts,
    source: Body,
    encoding: Option<&'static str>,
    min: usize,
) -> Response {
    let collected = match source.collect().await {
        Ok(collected) => collected,
        Err(error) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            return Response::from_parts(
                parts,
                Body::from_stream(futures_util::stream::once(
                    async move { Err::<Bytes, _>(error) },
                )),
            );
        }
    };
    let trailers = collected.trailers().cloned();
    let bytes = collected.to_bytes();
    let encoding = encoding.filter(|_| bytes.len() >= min);
    if let Some(trailers) = trailers {
        let body = Body::new(
            http_body_util::Full::new(bytes)
                .with_trailers(async move { Some(Ok::<_, std::convert::Infallible>(trailers)) }),
        );
        return encoded(parts, body, encoding);
    }
    let wire = if let Some(encoding) = encoding {
        let Ok(mut encoder) = Encoder::new(encoding) else {
            return fixed_response(parts, bytes);
        };
        let mut wire = Vec::new();
        for chunk in bytes.chunks(CHUNK) {
            match encoder.chunk(chunk) {
                Ok(chunk) => wire.extend_from_slice(&chunk),
                Err(_) => return fixed_response(parts, bytes),
            }
        }
        match encoder.finish() {
            Ok(tail) => wire.extend_from_slice(&tail),
            Err(_) => return fixed_response(parts, bytes),
        }
        parts
            .headers
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
        Bytes::from(wire)
    } else {
        bytes
    };
    fixed_response(parts, wire)
}

fn fixed_response(mut parts: http::response::Parts, bytes: Bytes) -> Response {
    parts
        .headers
        .insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    Response::from_parts(parts, Body::from(bytes))
}

fn encoded(
    mut parts: http::response::Parts,
    source: Body,
    encoding: Option<&'static str>,
) -> Response {
    let Some(encoding) = encoding else {
        return Response::from_parts(parts, source);
    };
    let Ok(encoder) = Encoder::new(encoding) else {
        return Response::from_parts(parts, source);
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    parts
        .headers
        .insert(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
    Response::from_parts(
        parts,
        Body::new(CompressedBody {
            source: Some(source),
            encoder: Some(encoder),
            pending: Bytes::new(),
            trailer: None,
            done: false,
            panic: None,
        }),
    )
}
use futures_util::StreamExt;

enum Codec {
    Gzip(Box<flate2::write::GzEncoder<Vec<u8>>>),
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
}
struct Encoder {
    codec: Codec,
    #[cfg(test)]
    lifetime: std::sync::Arc<()>,
}
impl Encoder {
    fn new(name: &str) -> io::Result<Self> {
        let codec = if name == "gzip" {
            Codec::Gzip(Box::new(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::default(),
            )))
        } else {
            let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1)?;
            encoder.window_log(ZSTD_WINDOW_LOG)?;
            Codec::Zstd(encoder)
        };
        Ok(Self {
            codec,
            #[cfg(test)]
            lifetime: std::sync::Arc::new(()),
        })
    }
    fn chunk(&mut self, input: &[u8]) -> io::Result<Bytes> {
        match &mut self.codec {
            Codec::Gzip(e) => {
                e.write_all(input)?;
                e.flush()?;
                Ok(Bytes::from(std::mem::take(e.get_mut())))
            }
            Codec::Zstd(e) => {
                e.write_all(input)?;
                e.flush()?;
                Ok(Bytes::from(std::mem::take(e.get_mut())))
            }
        }
    }
    fn finish(self) -> io::Result<Bytes> {
        match self.codec {
            Codec::Gzip(e) => (*e).finish().map(Bytes::from),
            Codec::Zstd(e) => e.finish().map(Bytes::from),
        }
    }
}
struct CompressedBody {
    source: Option<Body>,
    encoder: Option<Encoder>,
    pending: Bytes,
    trailer: Option<Frame<Bytes>>,
    done: bool,
    panic: Option<Box<dyn std::any::Any + Send>>,
}
impl HttpBody for CompressedBody {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        if self.done {
            if let Some(panic) = self.panic.take() {
                std::panic::resume_unwind(panic);
            }
            return Poll::Ready(self.trailer.take().map(Ok));
        }
        loop {
            if !self.pending.is_empty() {
                let len = self.pending.len().min(CHUNK);
                let input = self.pending.split_to(len);
                match self.encoder.as_mut().expect("active encoder").chunk(&input) {
                    Ok(bytes) => return Poll::Ready(Some(Ok(Frame::data(bytes)))),
                    Err(error) => {
                        self.release();
                        return Poll::Ready(Some(Err(error)));
                    }
                }
            }
            // Finish a committed compressed frame before resuming a handler body's panic.
            let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Pin::new(self.source.as_mut().expect("active source")).poll_frame(cx)
            }));
            let polled = match polled {
                Ok(polled) => polled,
                Err(panic) => {
                    self.panic = Some(panic);
                    break;
                }
            };
            match polled {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) => self.pending = data,
                    Err(trailer) => {
                        self.trailer = Some(trailer);
                        break;
                    }
                },
                Poll::Ready(Some(Err(error))) => {
                    self.release();
                    return Poll::Ready(Some(Err(io::Error::other(error))));
                }
                Poll::Ready(None) => break,
            }
        }
        let tail = self.encoder.take().expect("active encoder").finish();
        self.source = None;
        self.pending = Bytes::new();
        self.done = true;
        Poll::Ready(Some(tail.map(Frame::data)))
    }
}
impl CompressedBody {
    fn release(&mut self) {
        self.encoder = None;
        self.source = None;
        self.pending = Bytes::new();
        self.trailer = None;
        self.done = true;
        self.panic = None;
    }
}
#[cfg(test)]
mod tests;
