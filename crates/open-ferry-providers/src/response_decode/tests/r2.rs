use super::*;
#[tokio::test]
async fn r2_gzip_header_crc() {
    let wire: Vec<u8> = "1f8b08020000000000ff90c9cb48cdc9c9070086a6103605000000"
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|v| u8::from_str_radix(std::str::from_utf8(v).unwrap(), 16).unwrap())
        .collect();
    for label in ["gzip", ""] {
        for cut in 0..=wire.len() {
            let chunks = [
                Ok::<_, io::Error>(Bytes::copy_from_slice(&wire[..cut])),
                Ok(Bytes::copy_from_slice(&wire[cut..])),
            ];
            assert_eq!(
                collect(
                    tokio_util::io::StreamReader::new(stream::iter(chunks)),
                    label,
                    100
                )
                .await,
                (b"hello".to_vec(), 0),
                "cut {cut}"
            );
        }
    }
    let mut bad = wire.clone();
    bad[10] ^= 1;
    assert_eq!(collect(std::io::Cursor::new(bad), "gzip", 100).await.1, 1);
    assert_eq!(collect(std::io::Cursor::new(wire), "gzip", 4).await.1, 1);
}
#[tokio::test]
async fn r2_brotli_trailing_data() {
    for trailing in [0, 6] {
        let wire = vec![6, trailing];
        for cut in 0..=2 {
            let chunks = [
                Ok::<_, io::Error>(Bytes::copy_from_slice(&wire[..cut])),
                Ok(Bytes::copy_from_slice(&wire[cut..])),
            ];
            assert_eq!(
                collect(
                    tokio_util::io::StreamReader::new(stream::iter(chunks)),
                    "br",
                    100
                )
                .await
                .1,
                1,
                "trailing {trailing} cut {cut}"
            );
        }
        let wire = encode("zstd", &wire).await;
        assert_eq!(
            collect(std::io::Cursor::new(wire), "br, zstd", 100).await.1,
            1
        );
    }
    assert_eq!(
        collect(std::io::Cursor::new(vec![6]), "br", 100).await,
        (vec![], 0)
    );
}
#[tokio::test]
async fn r2_bound_mode_uses_cut_boundary() {
    let (bytes, errors, chunks) = bound_mode(&[b'x'; 32], "", 3).await;
    assert_eq!(bytes, vec![b'x'; 7]);
    assert_eq!(errors, 1);
    let mut total = 0;
    assert!(
        chunks.into_iter().any(|n| {
            total += n;
            total == 3
        }),
        "bound mode must observe its input cut"
    );
}
