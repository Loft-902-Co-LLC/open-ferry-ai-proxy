//! Lazy, synchronous-codec upstream decoding. Dropping the stream cancels and closes its source.
//! Claude deliberately does not use this decoder.
use async_compression::tokio::bufread::{
    BrotliDecoder, DeflateDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder,
};
use bytes::Bytes;
use futures_util::{Stream, stream};
use http::{HeaderMap, header};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader, ReadBuf};

type Reader = Pin<Box<dyn AsyncRead + Send>>;
const CHUNK: usize = 16 * 1024;
/// Maximum number of declared codec layers (bounds decoder working memory).
pub const MAX_ENCODING_LAYERS: usize = 8;

/// Decodes a Codex response reader, with `limit` bytes allowed at every stage.
/// Construction never reads. Unsupported/invalid encodings, source errors and size
/// violations produce exactly one `io::Error`, then EOF. Drop closes all readers;
/// there are no background tasks. Zstd windows are capped at 8 MiB before allocation.
/// Adapt `response.bytes_stream()` with `tokio_util::io::StreamReader` first.
pub fn decode_response_body<R: AsyncRead + Send + 'static>(
    body: R,
    headers: &HeaderMap,
    limit: usize,
) -> impl Stream<Item = io::Result<Bytes>> + Send + use<R> {
    let encoding = headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .map(|v| v.to_str().unwrap_or("invalid"))
        .collect::<Vec<_>>()
        .join(",");
    stream::try_unfold(
        (Some((Box::pin(body) as Reader, encoding)), None::<Reader>),
        move |(init, reader)| async move {
            let mut reader = match init {
                Some((body, encoding)) => prepare(body, &encoding, limit).await?,
                None => reader.expect("active decoder"),
            };
            let mut output = vec![0; CHUNK.min(limit.saturating_add(1))];
            let n = reader.read(&mut output).await?;
            if n == 0 {
                return Ok(None);
            }
            output.truncate(n);
            Ok(Some((Bytes::from(output), (None, Some(reader)))))
        },
    )
}

async fn prefix(mut reader: Reader, count: usize) -> io::Result<(Vec<u8>, Reader)> {
    let mut prefix = Vec::with_capacity(count);
    while prefix.len() < count {
        let mut byte = [0];
        if reader.read(&mut byte).await? == 0 {
            break;
        }
        prefix.push(byte[0]);
    }
    Ok((
        prefix.clone(),
        Box::pin(std::io::Cursor::new(prefix).chain(reader)),
    ))
}

async fn prepare(mut reader: Reader, encoding: &str, limit: usize) -> io::Result<Reader> {
    let mut names: Vec<String> = encoding
        .split(',')
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    if encoding.is_empty() {
        let (first, replay) = prefix(reader, 1).await?;
        reader = replay;
        if matches!(first.first(), Some(0x1f | 0x28)) {
            let (magic, replay) = prefix(reader, 4).await?;
            reader = replay;
            if magic.starts_with(&[0x1f, 0x8b]) {
                names = vec!["gzip".into()];
            } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
                names = vec!["zstd".into()];
            }
        }
    }
    if names.len() > MAX_ENCODING_LAYERS {
        return Err(io::Error::other("too many response encoding layers"));
    }
    for name in names.iter().rev() {
        reader = match name.as_str() {
            "" | "identity" => reader,
            "gzip" => {
                let mut decoder = GzipDecoder::new(BufReader::new(reader));
                decoder.multiple_members(true);
                Box::pin(decoder)
            }
            "zstd" => {
                let mut decoder = ZstdDecoder::with_params(
                    BufReader::new(reader),
                    &[async_compression::codecs::zstd::params::DParameter::window_log_max(23)],
                );
                decoder.multiple_members(true);
                Box::pin(decoder)
            }
            "br" => Box::pin(BrotliDecoder::new(BufReader::new(reader))),
            "deflate" => {
                let (head, replay) = prefix(reader, 2).await?;
                let zlib = head.len() == 2
                    && head[0] & 15 == 8
                    && head[0] >> 4 <= 7
                    && u16::from_be_bytes([head[0], head[1]]).is_multiple_of(31);
                if zlib {
                    Box::pin(ZlibDecoder::new(BufReader::new(replay)))
                } else {
                    Box::pin(DeflateDecoder::new(BufReader::new(replay)))
                }
            }
            _ => {
                return Err(io::Error::other(format!(
                    "unsupported content encoding {name:?}"
                )));
            }
        };
        reader = Box::pin(Limited {
            reader,
            remaining: limit,
        });
    }
    Ok(Box::pin(Limited {
        reader,
        remaining: limit,
    }))
}

struct Limited {
    reader: Reader,
    remaining: usize,
}
impl AsyncRead for Limited {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut scratch = [0; CHUNK];
        let max = output.remaining().min(CHUNK).min(self.remaining.max(1));
        let mut buf = ReadBuf::new(&mut scratch[..max]);
        match self.reader.as_mut().poll_read(cx, &mut buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {
                let n = buf.filled().len();
                if n > self.remaining {
                    return Poll::Ready(Err(io::Error::other("decoded response exceeds limit")));
                }
                self.remaining -= n;
                output.put_slice(buf.filled());
                Poll::Ready(Ok(()))
            }
        }
    }
}

#[cfg(test)]
mod tests;
