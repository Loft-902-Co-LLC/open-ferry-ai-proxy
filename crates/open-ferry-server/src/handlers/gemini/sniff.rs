// Ported from DetectContentType in Go's net/http/sniff.go (go1.26.4,
// BSD-3-Clause), as Go's server applies it to CLIProxyAPI
// sdk/api/handlers/gemini/gemini_handlers.go's raw streams and as
// multipartFileToDataURL in sdk/api/handlers/openai/openai_images_handlers.go
// calls it (v8.0.15, MIT).
// Copyright 2011 The Go Authors. All rights reserved.

//! The `Content-Type` Go's server gives a response that sets none, from the
//! first bytes written (the WHATWG MIME sniffing algorithm, as Go has it).
//!
//! Deviations from upstream: none.

/// The most bytes the algorithm reads.
const SNIFF_LEN: usize = 512;

/// A pattern the first bytes may match.
enum Sig {
    /// An HTML tag, case-insensitive, after leading whitespace, followed by
    /// a space or `>`.
    Html(&'static [u8]),
    /// Bytes that, under a mask, equal a pattern.
    Masked {
        mask: &'static [u8],
        pat: &'static [u8],
        skip_ws: bool,
        ct: &'static str,
    },
    /// A prefix.
    Exact(&'static [u8], &'static str),
    /// An MP4 `ftyp` box naming an `mp4` brand.
    Mp4,
    /// Text: no binary control bytes after leading whitespace.
    Text,
}

/// Go's `sniffSignatures`, in its order.
const SIGNATURES: &[Sig] = &[
    Sig::Html(b"<!DOCTYPE HTML"),
    Sig::Html(b"<HTML"),
    Sig::Html(b"<HEAD"),
    Sig::Html(b"<SCRIPT"),
    Sig::Html(b"<IFRAME"),
    Sig::Html(b"<H1"),
    Sig::Html(b"<DIV"),
    Sig::Html(b"<FONT"),
    Sig::Html(b"<TABLE"),
    Sig::Html(b"<A"),
    Sig::Html(b"<STYLE"),
    Sig::Html(b"<TITLE"),
    Sig::Html(b"<B"),
    Sig::Html(b"<BODY"),
    Sig::Html(b"<BR"),
    Sig::Html(b"<P"),
    Sig::Html(b"<!--"),
    Sig::Masked {
        mask: b"\xFF\xFF\xFF\xFF\xFF",
        pat: b"<?xml",
        skip_ws: true,
        ct: "text/xml; charset=utf-8",
    },
    Sig::Exact(b"%PDF-", "application/pdf"),
    Sig::Exact(b"%!PS-Adobe-", "application/postscript"),
    // UTF BOMs.
    Sig::Masked {
        mask: b"\xFF\xFF\x00\x00",
        pat: b"\xFE\xFF\x00\x00",
        skip_ws: false,
        ct: "text/plain; charset=utf-16be",
    },
    Sig::Masked {
        mask: b"\xFF\xFF\x00\x00",
        pat: b"\xFF\xFE\x00\x00",
        skip_ws: false,
        ct: "text/plain; charset=utf-16le",
    },
    Sig::Masked {
        mask: b"\xFF\xFF\xFF\x00",
        pat: b"\xEF\xBB\xBF\x00",
        skip_ws: false,
        ct: "text/plain; charset=utf-8",
    },
    // Images.
    Sig::Exact(b"\x00\x00\x01\x00", "image/x-icon"),
    Sig::Exact(b"\x00\x00\x02\x00", "image/x-icon"),
    Sig::Exact(b"BM", "image/bmp"),
    Sig::Exact(b"GIF87a", "image/gif"),
    Sig::Exact(b"GIF89a", "image/gif"),
    Sig::Masked {
        mask: b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF\xFF\xFF",
        pat: b"RIFF\x00\x00\x00\x00WEBPVP",
        skip_ws: false,
        ct: "image/webp",
    },
    Sig::Exact(b"\x89PNG\x0D\x0A\x1A\x0A", "image/png"),
    Sig::Exact(b"\xFF\xD8\xFF", "image/jpeg"),
    // Audio and video.
    Sig::Masked {
        mask: b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
        pat: b"FORM\x00\x00\x00\x00AIFF",
        skip_ws: false,
        ct: "audio/aiff",
    },
    Sig::Masked {
        mask: b"\xFF\xFF\xFF",
        pat: b"ID3",
        skip_ws: false,
        ct: "audio/mpeg",
    },
    Sig::Masked {
        mask: b"\xFF\xFF\xFF\xFF\xFF",
        pat: b"OggS\x00",
        skip_ws: false,
        ct: "application/ogg",
    },
    Sig::Masked {
        mask: b"\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF",
        pat: b"MThd\x00\x00\x00\x06",
        skip_ws: false,
        ct: "audio/midi",
    },
    Sig::Masked {
        mask: b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
        pat: b"RIFF\x00\x00\x00\x00AVI ",
        skip_ws: false,
        ct: "video/avi",
    },
    Sig::Masked {
        mask: b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
        pat: b"RIFF\x00\x00\x00\x00WAVE",
        skip_ws: false,
        ct: "audio/wave",
    },
    Sig::Mp4,
    Sig::Exact(b"\x1A\x45\xDF\xA3", "video/webm"),
    // Fonts.
    Sig::Masked {
        // 34 zero bytes, then "LP".
        mask: b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xFF\xFF",
        pat: b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00LP",
        skip_ws: false,
        ct: "application/vnd.ms-fontobject",
    },
    Sig::Exact(b"\x00\x01\x00\x00", "font/ttf"),
    Sig::Exact(b"OTTO", "font/otf"),
    Sig::Exact(b"ttcf", "font/collection"),
    Sig::Exact(b"wOFF", "font/woff"),
    Sig::Exact(b"wOF2", "font/woff2"),
    // Archives.
    Sig::Exact(b"\x1F\x8B\x08", "application/x-gzip"),
    Sig::Exact(b"PK\x03\x04", "application/zip"),
    Sig::Exact(b"Rar!\x1A\x07\x00", "application/x-rar-compressed"),
    Sig::Exact(b"Rar!\x1A\x07\x01\x00", "application/x-rar-compressed"),
    Sig::Exact(b"\x00\x61\x73\x6D", "application/wasm"),
    Sig::Text,
];

/// The MIME type of `data`, from at most its first 512 bytes, or
/// `application/octet-stream` (Go's `http.DetectContentType`).
pub(crate) fn detect_content_type(data: &[u8]) -> &'static str {
    let data = &data[..data.len().min(SNIFF_LEN)];
    let first_non_ws = data.iter().position(|&b| !is_ws(b)).unwrap_or(data.len());
    SIGNATURES
        .iter()
        .find_map(|sig| sig.matches(data, first_non_ws))
        .unwrap_or("application/octet-stream")
}

/// Whitespace as the algorithm defines it.
fn is_ws(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// A byte that ends a tag name.
fn is_tt(b: u8) -> bool {
    matches!(b, b' ' | b'>')
}

impl Sig {
    /// The MIME type when `data` matches.
    fn matches(&self, data: &[u8], first_non_ws: usize) -> Option<&'static str> {
        match self {
            Sig::Exact(sig, ct) => data.starts_with(sig).then_some(*ct),
            Sig::Masked {
                mask,
                pat,
                skip_ws,
                ct,
            } => {
                let data = if *skip_ws {
                    &data[first_non_ws..]
                } else {
                    data
                };
                if pat.len() != mask.len() || data.len() < pat.len() {
                    return None;
                }
                let matched = pat
                    .iter()
                    .zip(mask.iter())
                    .zip(data)
                    .all(|((p, m), d)| d & m == *p);
                matched.then_some(*ct)
            }
            Sig::Html(tag) => {
                let data = &data[first_non_ws..];
                if data.len() < tag.len() + 1 {
                    return None;
                }
                let matched = tag.iter().zip(data).all(|(&b, &d)| {
                    let d = if b.is_ascii_uppercase() { d & 0xDF } else { d };
                    b == d
                });
                (matched && is_tt(data[tag.len()])).then_some("text/html; charset=utf-8")
            }
            Sig::Mp4 => mp4(data),
            Sig::Text => data[first_non_ws..]
                .iter()
                .all(|&b| !matches!(b, 0x00..=0x08 | 0x0B | 0x0E..=0x1A | 0x1C..=0x1F))
                .then_some("text/plain; charset=utf-8"),
        }
    }
}

/// `video/mp4` when `data` starts with an `ftyp` box naming an `mp4`
/// brand, skipping the major brand's version.
fn mp4(data: &[u8]) -> Option<&'static str> {
    if data.len() < 12 {
        return None;
    }
    let box_size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < box_size || !box_size.is_multiple_of(4) {
        return None;
    }
    if &data[4..8] != b"ftyp" {
        return None;
    }
    (8..box_size)
        .step_by(4)
        .filter(|&st| st != 12)
        .any(|st| &data[st..st + 3] == b"mp4")
        .then_some("video/mp4")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each answer is what Go 1.26.4's DetectContentType gives.
    #[test]
    fn detects_as_go_does() {
        let cases: &[(&[u8], &str)] = &[
            (b"", "text/plain; charset=utf-8"),
            (b"\x00\x01\x02", "application/octet-stream"),
            (b"Hello, world!", "text/plain; charset=utf-8"),
            (br#"[{"n":1}"#, "text/plain; charset=utf-8"),
            (b"  <html><body>", "text/html; charset=utf-8"),
            (b"<HtMl>", "text/html; charset=utf-8"),
            (b"<html", "text/plain; charset=utf-8"),
            (b"<!-- x -->", "text/html; charset=utf-8"),
            (b"\n<?xml!", "text/xml; charset=utf-8"),
            (b"%PDF-1.4", "application/pdf"),
            (b"%!PS-Adobe-3.0", "application/postscript"),
            (b"\xFE\xFF\x00\x41", "text/plain; charset=utf-16be"),
            (b"\xFF\xFE\x41\x00", "text/plain; charset=utf-16le"),
            (b"\xEF\xBB\xBFhi", "text/plain; charset=utf-8"),
            (b"GIF89a...", "image/gif"),
            (b"\x89PNG\x0D\x0A\x1A\x0A", "image/png"),
            (b"\xFF\xD8\xFF\xE0", "image/jpeg"),
            (b"RIFF\x01\x02\x03\x04WEBPVP8 ", "image/webp"),
            (b"RIFF\x01\x02\x03\x04WAVEfmt ", "audio/wave"),
            (b"ID3\x03", "audio/mpeg"),
            (b"OggS\x00\x02", "application/ogg"),
            (b"\x1A\x45\xDF\xA3", "video/webm"),
            (
                b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom",
                "video/mp4",
            ),
            (b"\x00\x00\x00\x10ftypisommp41", "application/octet-stream"),
            (b"wOF2", "font/woff2"),
            (b"\x1F\x8B\x08\x00", "application/x-gzip"),
            (b"PK\x03\x04", "application/zip"),
            (b"Rar!\x1A\x07\x01\x00", "application/x-rar-compressed"),
            (b"\x00asm\x01", "application/wasm"),
            (b"text\x1b", "text/plain; charset=utf-8"),
            (b"text\x1c", "application/octet-stream"),
        ];
        for (data, want) in cases {
            assert_eq!(detect_content_type(data), *want, "{data:?}");
        }
        let mut eot = vec![0u8; 34];
        eot.extend_from_slice(b"LP");
        assert_eq!(detect_content_type(&eot), "application/vnd.ms-fontobject");
        // Only the first 512 bytes count.
        let mut long = vec![b'a'; 512];
        long.push(0);
        assert_eq!(detect_content_type(&long), "text/plain; charset=utf-8");
    }
}
