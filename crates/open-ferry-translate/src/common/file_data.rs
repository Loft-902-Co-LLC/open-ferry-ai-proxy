// Ported from CLIProxyAPI internal/translator/common/file_data.go (NormalizeOpenAIFileData)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Reads the file data OpenAI clients send, either a `data:` URL or bare
//! base64, into a MIME type and a base64 payload.
//!
//! Deviations from upstream: none.

use crate::common::mime_types::mime_type;
use crate::go;

const DATA_URL_PREFIX: &str = "data:";

/// A file's MIME type and its base64 payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileData {
    pub(crate) mime_type: String,
    pub(crate) data: String,
}

/// The MIME type and base64 payload of `file_data`: a `data:` URL marked
/// `base64`, or bare base64 typed by `fallback_mime_type`, or else by the
/// extension of `filename`. `None` if it is empty, or the type can't be told.
pub(crate) fn normalize_openai_file_data(
    filename: &str,
    fallback_mime_type: &str,
    file_data: &str,
) -> Option<FileData> {
    if file_data.is_empty() {
        return None;
    }
    let fallback = if fallback_mime_type.is_empty() {
        mime_type(&go::to_lower(extension(filename))).unwrap_or_default()
    } else {
        fallback_mime_type
    };

    // "data:" is ASCII and holds no letter another character folds to, so
    // comparing bytes without case is Go's EqualFold here.
    let is_data_url = file_data
        .as_bytes()
        .get(..DATA_URL_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(DATA_URL_PREFIX.as_bytes()));
    if !is_data_url {
        return (!fallback.is_empty()).then(|| FileData {
            mime_type: fallback.to_owned(),
            data: file_data.to_owned(),
        });
    }

    let (metadata, payload) = file_data[DATA_URL_PREFIX.len()..].split_once(',')?;
    if payload.is_empty() {
        return None;
    }
    let mut fields = metadata.split(';');
    let mime_type = fields.next().unwrap_or_default().trim();
    if mime_type.is_empty() {
        return None;
    }
    fields
        .any(|field| equal_fold_ascii(field.trim(), "base64"))
        .then(|| FileData {
            mime_type: mime_type.to_owned(),
            data: payload.to_owned(),
        })
}

/// Go's `filepath.Ext` without its leading dot: what follows the last dot of
/// the last path element, or `""`.
fn extension(filename: &str) -> &str {
    let name = filename.rsplit('/').next().unwrap_or_default();
    name.rsplit_once('.').map_or("", |(_, extension)| extension)
}

/// Go's `strings.EqualFold` against an ASCII `word`. Two non-ASCII characters
/// fold to ASCII letters: the long s to `s` and the Kelvin sign to `k`.
fn equal_fold_ascii(text: &str, word: &str) -> bool {
    let mut chars = text.chars();
    let folds = word.chars().all(|w| {
        chars.next().is_some_and(|c| {
            c.eq_ignore_ascii_case(&w)
                || match w.to_ascii_lowercase() {
                    's' => c == '\u{17f}',
                    'k' => c == '\u{212a}',
                    _ => false,
                }
        })
    });
    folds && chars.next().is_none()
}

#[cfg(test)]
mod tests {
    //! Ports internal/translator/common/file_data_test.go, and adds tests of
    //! the case folding and extensions.

    use super::*;

    fn normalize(filename: &str, fallback: &str, data: &str) -> Option<(String, String)> {
        normalize_openai_file_data(filename, fallback, data).map(|file| (file.mime_type, file.data))
    }

    fn found(mime_type: &str, data: &str) -> Option<(String, String)> {
        Some((mime_type.to_owned(), data.to_owned()))
    }

    #[test]
    fn normalize_openai_file_data_cases() {
        let cases = [
            (
                "data URL",
                "test.pdf",
                "",
                "data:application/pdf;base64,JVBERi0xLjQK",
                found("application/pdf", "JVBERi0xLjQK"),
            ),
            (
                "data URL metadata and MIME override",
                "test.txt",
                "",
                "data:application/pdf;charset=binary;BASE64,JVBERi0xLjQK",
                found("application/pdf", "JVBERi0xLjQK"),
            ),
            (
                "case-insensitive data URL scheme",
                "test.pdf",
                "",
                "DATA:application/pdf;base64,JVBERi0xLjQK",
                found("application/pdf", "JVBERi0xLjQK"),
            ),
            (
                "raw base64",
                "TEST.PDF",
                "",
                "JVBERi0xLjQK",
                found("application/pdf", "JVBERi0xLjQK"),
            ),
            (
                "raw base64 with explicit MIME type",
                "",
                "application/pdf",
                "JVBERi0xLjQK",
                found("application/pdf", "JVBERi0xLjQK"),
            ),
            ("empty data", "test.pdf", "", "", None),
            (
                "raw base64 without known extension",
                "test",
                "",
                "JVBERi0xLjQK",
                None,
            ),
            (
                "data URL without base64 marker",
                "test.pdf",
                "",
                "data:application/pdf,JVBERi0xLjQK",
                None,
            ),
            (
                "data URL without MIME type",
                "test.pdf",
                "",
                "data:;base64,JVBERi0xLjQK",
                None,
            ),
            (
                "data URL without payload",
                "test.pdf",
                "",
                "data:application/pdf;base64,",
                None,
            ),
        ];
        for (name, filename, fallback, data, want) in cases {
            assert_eq!(normalize(filename, fallback, data), want, "{name}");
        }
    }

    #[test]
    fn base64_marker_folds_case_as_go_does() {
        let long_s = format!("data:text/plain; ba{}e64 ,QQ==", '\u{17f}');
        assert_eq!(normalize("", "", &long_s), found("text/plain", "QQ=="));
        assert_eq!(
            normalize("", "", "data: text/plain ;Base64,QQ=="),
            found("text/plain", "QQ==")
        );
        assert_eq!(normalize("", "", "data:text/plain;base6,QQ=="), None);
        assert_eq!(normalize("", "", "data:text/plain;base644,QQ=="), None);
        // The payload is everything after the first comma.
        assert_eq!(
            normalize("", "", "data:a/b;base64,x,y"),
            found("a/b", "x,y")
        );
        // A short value is never a data URL.
        assert_eq!(normalize("x.png", "", "data"), found("image/png", "data"));
    }

    #[test]
    fn extension_is_that_of_the_last_path_element() {
        assert_eq!(extension("dir.d/file.tar.GZ"), "GZ");
        assert_eq!(extension("dir.d/file"), "");
        assert_eq!(extension("file."), "");
        assert_eq!(extension(""), "");
        assert_eq!(
            normalize("a.pdf/notes", "", "JVBE"),
            None,
            "the dot is in a directory name"
        );
        let kelvin = format!("map.{}MZ", '\u{212a}');
        assert_eq!(
            normalize(&kelvin, "", "UEsD"),
            found("application/vnd.google-earth.kmz", "UEsD")
        );
    }

    #[test]
    fn equal_fold_ascii_matches_whole_words() {
        assert!(equal_fold_ascii("BASE64", "base64"));
        assert!(!equal_fold_ascii("base64x", "base64"));
        assert!(!equal_fold_ascii("base6", "base64"));
        assert!(equal_fold_ascii(&format!("{}ind", '\u{212a}'), "kind"));
        assert!(!equal_fold_ascii("\u{e9}", "e"));
    }
}
