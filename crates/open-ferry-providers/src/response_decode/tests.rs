use super::*;
use futures_util::{StreamExt, TryStreamExt};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::AsyncWriteExt;

async fn encode(name: &str, input: &[u8]) -> Vec<u8> {
    match name {
        "gzip" => {
            use std::io::Write;
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(input).unwrap();
            e.finish().unwrap()
        }
        "zstd" => {
            let mut e = async_compression::tokio::write::ZstdEncoder::new(Vec::new());
            e.write_all(input).await.unwrap();
            e.shutdown().await.unwrap();
            e.into_inner()
        }
        _ => unreachable!(),
    }
}
async fn collect(
    reader: impl AsyncRead + Send + 'static,
    label: &str,
    bound: usize,
) -> (Vec<u8>, usize) {
    let mut headers = HeaderMap::new();
    if !label.is_empty() {
        headers.insert(header::CONTENT_ENCODING, label.parse().unwrap());
    }
    let stream = decode_response_body(reader, &headers, bound);
    futures_util::pin_mut!(stream);
    let mut output = Vec::new();
    let mut errors = 0;
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => output.extend_from_slice(&bytes),
            Err(_) => errors += 1,
        }
        assert!(output.len() <= bound);
    }
    (output, errors)
}

// The same invariant applies to every chain, size, chunk boundary and cancel:
const INVARIANT: &str =
    "original bytes or exactly one error; output <= bound; drop releases source";
#[tokio::test]
async fn invariant_codec_size_cut_cancel_family() {
    let mut cases = 0;
    for size in [0, 31, 1024, 32768] {
        let plain: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        for chain in ["", "gzip", "zstd", "gzip, zstd", "unlabeled zstd"] {
            let mut wire = plain.clone();
            let label = if chain == "unlabeled zstd" { "" } else { chain };
            for codec in chain
                .replace("unlabeled ", "")
                .split(", ")
                .filter(|s| !s.is_empty())
            {
                wire = encode(codec, &wire).await;
            }
            let bound = plain.len().max(wire.len()).saturating_add(1024);
            let cuts: Vec<usize> = (0..=wire.len())
                .step_by(127)
                .chain(std::iter::once(wire.len()))
                .collect();
            for cut in cuts {
                let chunks = vec![
                    Ok::<_, io::Error>(Bytes::copy_from_slice(&wire[..cut])),
                    Ok(Bytes::copy_from_slice(&wire[cut..])),
                ];
                let reader = tokio_util::io::StreamReader::new(stream::iter(chunks));
                let (got, errors) = collect(reader, label, bound).await;
                assert!(
                    got == plain || errors == 1,
                    "{INVARIANT}: {chain}/{size}/{cut}"
                );
                assert_eq!(errors, 0, "valid frame {chain}/{size}/{cut}");
                let (got, errors, _) = bound_mode(&wire, label, cut).await;
                assert!(got == plain || errors == 1, "{INVARIANT}");
                let cancel_drops = Arc::new(AtomicUsize::new(0));
                let canceled = tokio_util::io::StreamReader::new(stream::iter([
                    Ok(Bytes::copy_from_slice(&wire[..cut])),
                    Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "mock upstream cancellation",
                    )),
                ]));
                let canceled = Observed {
                    reader: canceled,
                    reads: Arc::new(AtomicUsize::new(0)),
                    drops: cancel_drops.clone(),
                };
                let (_, errors) = collect(canceled, label, bound).await;
                assert_eq!(errors, 1, "{INVARIANT}: canceled {chain}/{size}/{cut}");
                assert_eq!(cancel_drops.load(Ordering::SeqCst), 1, "{INVARIANT}");
                let drops = Arc::new(AtomicUsize::new(0));
                let reader = CancelReader {
                    data: std::io::Cursor::new(wire[..cut].to_vec()),
                    drops: drops.clone(),
                };
                let mut headers = HeaderMap::new();
                if !label.is_empty() {
                    headers.insert(header::CONTENT_ENCODING, label.parse().unwrap());
                }
                let mut decoder = Box::pin(decode_response_body(reader, &headers, bound));
                // Drain whatever the cut makes available, without waiting for the rest.
                while let Poll::Ready(Some(_)) = futures_util::poll!(decoder.next()) {}
                drop(decoder);
                assert_eq!(drops.load(Ordering::SeqCst), 1, "{INVARIANT}");
                cases += 1;
            }
        }
    }
    eprintln!(
        "invariant family: {cases} boundary cases, each success/bound/source-cancel/drop-cancel"
    );
}
struct CancelReader {
    data: std::io::Cursor<Vec<u8>>,
    drops: Arc<AtomicUsize>,
}
impl Drop for CancelReader {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl AsyncRead for CancelReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.data.position() == self.data.get_ref().len() as u64 {
            Poll::Pending
        } else {
            Pin::new(&mut self.data).poll_read(cx, out)
        }
    }
}
#[tokio::test]
async fn plain_first_byte_does_not_wait_for_magic_and_setup_is_lazy() {
    let drops = Arc::new(AtomicUsize::new(0));
    let reader = CancelReader {
        data: std::io::Cursor::new(b"d".to_vec()),
        drops: drops.clone(),
    };
    let decoder = decode_response_body(reader, &HeaderMap::new(), 100);
    futures_util::pin_mut!(decoder);
    let Poll::Ready(Some(first)) = futures_util::poll!(decoder.next()) else {
        panic!("plain first byte must not wait for four magic bytes");
    };
    assert_eq!(first.unwrap(), "d");
    let pending = decode_response_body(
        CancelReader {
            data: std::io::Cursor::new(vec![]),
            drops: drops.clone(),
        },
        &HeaderMap::new(),
        100,
    );
    drop(pending);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn split_magic_and_invalid_data_error_once() {
    for codec in ["gzip", "zstd"] {
        let wire = encode(codec, b"data: done\n\n").await;
        let chunks: Vec<_> = wire
            .into_iter()
            .map(|b| Ok::<_, io::Error>(Bytes::from(vec![b])))
            .collect();
        let reader = tokio_util::io::StreamReader::new(stream::iter(chunks));
        assert_eq!(
            collect(reader, "", 1024).await,
            (b"data: done\n\n".to_vec(), 0)
        );
        assert_eq!(
            collect(std::io::Cursor::new(vec![1, 2, 3]), codec, 100)
                .await
                .1,
            1
        );
    }
    for plain in [vec![], vec![0x28], vec![0x1f], vec![1, 2, 3]] {
        assert_eq!(
            collect(std::io::Cursor::new(plain.clone()), "", 100).await,
            (plain, 0)
        );
    }
    let _ = decode_response_body(
        std::io::Cursor::new(b"hello".to_vec()),
        &HeaderMap::new(),
        99,
    )
    .try_collect::<Vec<_>>()
    .await
    .unwrap();
}
#[tokio::test]
async fn deflate_both_wrappers_brotli_and_unknown_encoding() {
    use std::io::Write;
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    zlib.write_all(b"hello").unwrap();
    let mut raw = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    raw.write_all(b"hello").unwrap();
    for wire in [zlib.finish().unwrap(), raw.finish().unwrap()] {
        assert_eq!(
            collect(std::io::Cursor::new(wire), "deflate", 100).await,
            (b"hello".to_vec(), 0)
        );
    }
    let mut br = async_compression::tokio::write::BrotliEncoder::new(Vec::new());
    br.write_all(b"hello").await.unwrap();
    br.shutdown().await.unwrap();
    assert_eq!(
        collect(std::io::Cursor::new(br.into_inner()), "br", 100).await,
        (b"hello".to_vec(), 0)
    );
    assert_eq!(
        collect(std::io::Cursor::new(vec![0]), "unknown", 100)
            .await
            .1,
        1
    );
}
#[tokio::test]
async fn reverse_multiple_header_values_and_preallocation_window_limit() {
    let plain = b"data: response.completed\n\n";
    let gzip = encode("gzip", plain).await;
    let mut br = async_compression::tokio::write::BrotliEncoder::new(Vec::new());
    br.write_all(&gzip).await.unwrap();
    br.shutdown().await.unwrap();
    let wire = encode("zstd", &br.into_inner()).await;
    let mut headers = HeaderMap::new();
    headers.append(header::CONTENT_ENCODING, "GZIP, br".parse().unwrap());
    headers.append(header::CONTENT_ENCODING, "zstd".parse().unwrap());
    let parts = decode_response_body(std::io::Cursor::new(wire), &headers, 4096)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(parts.concat(), plain);
    // A valid empty frame declaring a 16 MiB window must fail before allocating it.
    let wire = vec![0x28, 0xb5, 0x2f, 0xfd, 0, 0x70, 1, 0, 0];
    assert_eq!(
        collect(std::io::Cursor::new(wire), "zstd", 4096).await,
        (vec![], 1)
    );
    assert_eq!(
        collect(
            std::io::Cursor::new(vec![]),
            &["zstd"; MAX_ENCODING_LAYERS + 1].join(","),
            4096
        )
        .await,
        (vec![], 1)
    );
}
struct Observed<R> {
    reader: R,
    reads: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}
impl<R> Drop for Observed<R> {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl<R: AsyncRead + Unpin> AsyncRead for Observed<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Pin::new(&mut self.reader).poll_read(cx, out)
    }
}
#[tokio::test]
async fn lazy_setup_eof_error_and_close_release_source_once() {
    for (label, valid) in [
        ("", true),
        ("gzip", true),
        ("zstd", true),
        ("gzip", false),
        ("zstd", false),
        ("unknown", false),
    ] {
        let wire = if valid && !label.is_empty() {
            encode(label, b"hello").await
        } else {
            b"hello".to_vec()
        };
        let drops = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let source = Observed {
            reader: std::io::Cursor::new(wire),
            reads: reads.clone(),
            drops: drops.clone(),
        };
        let mut headers = HeaderMap::new();
        if !label.is_empty() {
            headers.insert(header::CONTENT_ENCODING, label.parse().unwrap());
        }
        let mut decoder = Box::pin(decode_response_body(source, &headers, 1024));
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "construction must not read"
        );
        let mut errors = 0;
        while let Some(item) = decoder.next().await {
            if item.is_err() {
                errors += 1;
            }
        }
        assert_eq!(errors, usize::from(!valid));
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "EOF/error releases source even while stream is held"
        );
        assert!(decoder.next().await.is_none());
        drop(decoder);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn source_errors_are_forwarded_once() {
    let source = tokio_util::io::StreamReader::new(stream::iter([Err::<Bytes, _>(
        io::Error::new(io::ErrorKind::ConnectionReset, "mock upstream reset"),
    )]));
    let mut decoder = Box::pin(decode_response_body(source, &HeaderMap::new(), 1024));
    assert_eq!(
        decoder.next().await.unwrap().unwrap_err().kind(),
        io::ErrorKind::ConnectionReset
    );
    assert!(decoder.next().await.is_none());
}
#[tokio::test]
async fn layer_budget_invariant_family_ignores_identity() {
    for size in [0, 31, 1024] {
        let plain = vec![b'x'; size];
        for depth in 0..=10 {
            let mut wire = plain.clone();
            let mut names = Vec::new();
            for layer in 0..depth {
                let codec = if layer % 2 == 0 { "gzip" } else { "zstd" };
                wire = encode(codec, &wire).await;
                names.extend(["identity", codec, "identity"]);
            }
            let (got, errors) = collect(std::io::Cursor::new(wire), &names.join(","), 4096).await;
            if depth <= 8 {
                assert_eq!(
                    (got, errors),
                    (plain.clone(), 0),
                    "{INVARIANT}: depth {depth}"
                );
            } else {
                assert_eq!(errors, 1, "{INVARIANT}: depth {depth}");
            }
        }
        assert_eq!(
            collect(
                std::io::Cursor::new(plain.clone()),
                &["identity"; 32].join(","),
                4096
            )
            .await,
            (plain, 0)
        );
    }
}
#[tokio::test]
async fn window_budget_invariant_family() {
    for log in 10..=30 {
        let descriptor = ((log - 10) << 3) as u8;
        let wire = vec![0x28, 0xb5, 0x2f, 0xfd, 0, descriptor, 1, 0, 0];
        let (got, errors) = collect(std::io::Cursor::new(wire), "zstd", 4096).await;
        assert!(got.is_empty(), "{INVARIANT}");
        assert_eq!(
            errors,
            usize::from(log > 23),
            "{INVARIANT}: window log {log}"
        );
    }
}
#[tokio::test]
async fn r2_brotli_large_window_refused() {
    for log in 10u8..=30 {
        // Large-window extension, followed by an empty last meta-block.
        let wire = vec![0x11, 0xc0 | log];
        let (got, errors) = collect(std::io::Cursor::new(wire), "br", 4096).await;
        assert!(got.is_empty(), "{INVARIANT}");
        assert_eq!(errors, 1, "{INVARIANT}: brotli window log {log}");
    }
}

mod r2;

async fn bound_mode(wire: &[u8], label: &str, cut: usize) -> (Vec<u8>, usize, Vec<usize>) {
    let chunks = [
        Ok::<_, io::Error>(Bytes::copy_from_slice(&wire[..cut])),
        Ok(Bytes::copy_from_slice(&wire[cut..])),
    ];
    let reader = tokio_util::io::StreamReader::new(stream::iter(chunks));
    let mut headers = HeaderMap::new();
    if !label.is_empty() {
        headers.insert(header::CONTENT_ENCODING, label.parse().unwrap());
    }
    let mut decoder = Box::pin(decode_response_body(reader, &headers, 7));
    let mut output = Vec::new();
    let mut errors = 0;
    let mut chunks = Vec::new();
    while let Some(item) = decoder.next().await {
        match item {
            Ok(bytes) => {
                chunks.push(bytes.len());
                output.extend_from_slice(&bytes);
            }
            Err(_) => errors += 1,
        }
        assert!(output.len() <= 7, "{INVARIANT}");
    }
    (output, errors, chunks)
}
