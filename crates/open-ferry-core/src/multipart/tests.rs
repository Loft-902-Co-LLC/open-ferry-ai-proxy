// Checks of the Go std ports in this module (go1.26, BSD-3-Clause, see
// licenses/Go-LICENSE). The expected values are what go1.26's
// mime/multipart gives for the same input.
// https://github.com/golang/go

//! Tests of reading and writing forms. None are upstream's: upstream
//! relies on Go's `mime/multipart`, whose results these check.

use bytes::Bytes;

use super::*;

/// What [`Reader::next_part`] gives for each part of `body`: its field and
/// file names, its data, and the error its data ended with; then the
/// error that ended the parts, or `EOF`.
fn parts(body: &[u8], boundary: &str) -> Vec<String> {
    let mut reader = Reader::new(Bytes::copy_from_slice(body), boundary.as_bytes());
    let mut out = Vec::new();
    loop {
        match reader.next_part() {
            Ok(Some(part)) => {
                let form = part.form_name();
                let file = part.file_name();
                let line = match part.read_all() {
                    Ok(data) => format!("{form}|{file}|{}", lossy(&data)),
                    Err(error) => format!("{form}|{file}|error {error}"),
                };
                out.push(line);
            }
            Ok(None) => {
                out.push("EOF".to_owned());
                return out;
            }
            Err(error) => {
                out.push(format!("error {error}"));
                return out;
            }
        }
    }
}

/// `body` read into a form with `max_memory`.
fn form(body: &[u8], boundary: &str, max_memory: i64) -> Result<Form, Error> {
    Reader::new(Bytes::copy_from_slice(body), boundary.as_bytes()).read_form(max_memory)
}

/// A part's data, as its `Debug` must never show it.
const SECRET: &str = "a secret prompt";

/// A form with a preamble, a field, two files (one with a Windows path and
/// one empty) and an epilogue.
const CRLF: &[u8] = b"preamble\r\n--b\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\na cat\r\n--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"C:\\\\dir\\\\x.png\"\r\nContent-Type: image/png\r\n\r\nPNGDATA\r\n--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"../y.png\"\r\n\r\n\r\n--b--\r\nepilogue";

// Not upstream's: Go's NextPart skips the preamble and epilogue and reads
// each part's names and data.
#[test]
fn reads_parts() {
    assert_eq!(
        parts(CRLF, "b"),
        [
            "prompt||a cat",
            "image|x.png|PNGDATA",
            "image|y.png|",
            "EOF"
        ]
    );
}

// Not upstream's: Go's ReadForm keeps fields and files by name, each
// file with its header.
#[test]
fn reads_a_form() {
    let form = form(CRLF, "b", MAX_FORM_MEMORY).unwrap();
    assert_eq!(form.value("prompt").unwrap().as_ref(), b"a cat");
    assert_eq!(form.value("image"), None);
    let files = form.files_of("image");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].filename, "x.png");
    assert_eq!(files[0].data.as_ref(), b"PNGDATA");
    assert_eq!(files[0].header.get_str("content-type"), "image/png");
    assert_eq!(files[1].filename, "y.png");
    assert!(files[1].data.is_empty());
    assert!(form.files_of("prompt").is_empty());
    let names: Vec<&str> = form.values().map(|(name, _)| name).collect();
    assert_eq!(names, ["prompt"]);
}

// Not upstream's: Go reads a form whose lines end in `\n` alone, as its
// first boundary line does.
#[test]
fn reads_bare_newlines() {
    let body = b"--b\nContent-Disposition: form-data; name=\"a\"\n\nhello\nworld\n--b\ncontent-disposition: form-data; name=\"a\"\n\nagain\n--b--\n";
    assert_eq!(parts(body, "b"), ["a||hello\nworld", "a||again", "EOF"]);
    let form = form(body, "b", MAX_FORM_MEMORY).unwrap();
    let values: Vec<&[u8]> = form
        .values()
        .flat_map(|(_, values)| values.iter().map(|v| v.as_ref()))
        .collect();
    assert_eq!(values, [b"hello\nworld".as_slice(), b"again"]);
}

// Not upstream's: boundary lines may end in spaces and tabs, data may hold
// lines that start like a boundary, and a part may have no header.
#[test]
fn reads_boundary_lookalikes() {
    let body =
        b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n--bx\r\n--b-x\r\n--b--\r\n";
    assert_eq!(parts(body, "b"), ["a||--bx\r\n--b-x", "EOF"]);
    let body = b"--b  \r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nv\r\n--b \t\r\n\r\nw\r\n--b--  \r\n";
    assert_eq!(parts(body, "b"), ["a||v", "||w", "EOF"]);
}

// Not upstream's: the errors of Go's NextPart, word for word.
#[test]
fn next_part_errors() {
    let cases: [(&[u8], &str, &[&str]); 13] = [
        (CRLF, "", &["error multipart: boundary is empty"]),
        (
            b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nhello",
            "b",
            &["a||error unexpected EOF", "error multipart: NextPart: EOF"],
        ),
        (b"just text", "b", &["error multipart: NextPart: EOF"]),
        (b"just text\r\n", "b", &["error multipart: NextPart: EOF"]),
        (
            b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nv\r\n--b\tjunk\r\n",
            "b",
            &[
                "a||v",
                "error multipart: expecting a new Part; got line \"--b\\tjunk\\r\\n\"",
            ],
        ),
        (
            b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n--b junk\r\n",
            "b",
            &[
                "a||",
                "error multipart: unexpected line in Next(): \"--b junk\\r\\n\"",
            ],
        ),
        (
            b"--b\r\nNo colon here\r\n\r\nv\r\n--b--\r\n",
            "b",
            &["error malformed MIME header: missing colon: \"No colon here\""],
        ),
        (
            b"--b\r\n Foo: bar\r\n\r\nv\r\n--b--\r\n",
            "b",
            &["error malformed MIME header initial line: \" Foo: bar\""],
        ),
        (
            b"--b\r\nBad\x01Key: v\r\n\r\nv\r\n--b--\r\n",
            "b",
            &["error malformed MIME header line: \"Bad\\x01Key: v\""],
        ),
        (
            b"--b\r\nKey: v\x01\r\n\r\nv\r\n--b--\r\n",
            "b",
            &["error malformed MIME header line: \"Key: v\\x01\""],
        ),
        (
            b"--b\r\nmy key: v\r\n\r\nv\r\n--b--\r\n",
            "b",
            &["||v", "EOF"],
        ),
        // A header the body ends in ends the parts, as the final boundary
        // does.
        (
            b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n",
            "b",
            &["EOF"],
        ),
        (b"", "b", &["error multipart: NextPart: EOF"]),
    ];
    for (body, boundary, want) in cases {
        assert_eq!(parts(body, boundary), want, "{}", lossy(body));
    }
}

// Not upstream's: a line of more than 4096 bytes between parts fills Go's
// buffer.
#[test]
fn long_line_fills_the_buffer() {
    let mut body = vec![b'x'; 5000];
    body.extend_from_slice(b"\r\n--b--\r\n");
    assert_eq!(
        parts(&body, "b"),
        ["error multipart: NextPart: bufio: buffer full"]
    );
    let mut body = vec![b'x'; 4000];
    body.extend_from_slice(b"\r\n--b--\r\n");
    assert_eq!(parts(&body, "b"), ["EOF"]);
}

// Not upstream's: Go's header reader joins folded lines, keeps repeated
// names in order and canonicalizes names.
#[test]
fn reads_headers() {
    let body = b"--b\r\nContent-Disposition: form-data;\r\n  name=\"a\"  \r\nX-Other: 1\r\nx-other: 2\r\n\r\nv\r\n--b--\r\n";
    let mut reader = Reader::new(Bytes::from_static(body), b"b");
    let part = reader.next_part().unwrap().unwrap();
    assert_eq!(part.form_name(), "a");
    assert_eq!(
        part.header.get("Content-Disposition"),
        Some(b"form-data; name=\"a\"".as_slice())
    );
    let other: Vec<&[Vec<u8>]> = part
        .header
        .iter()
        .filter(|(name, _)| *name == "X-Other")
        .map(|(_, values)| values)
        .collect();
    assert_eq!(other, [[b"1".to_vec(), b"2".to_vec()].as_slice()]);
    assert_eq!(canonical_key("content-TYPE"), "Content-Type");
    assert_eq!(canonical_key("my key"), "my key");
}

// Not upstream's: a quoted-printable part is decoded and loses its
// Content-Transfer-Encoding; a bad byte ends its data with Go's error.
#[test]
fn decodes_quoted_printable() {
    let body = "--b\r\nContent-Disposition: form-data; name=\"a\"\r\nContent-Transfer-Encoding: Quoted-Printable\r\n\r\ncaf=C3=A9 =\r\nau lait=3d\r\n--b--\r\n";
    let mut reader = Reader::new(Bytes::from_static(body.as_bytes()), b"b");
    let part = reader.next_part().unwrap().unwrap();
    assert_eq!(part.header.get("Content-Transfer-Encoding"), None);
    assert_eq!(
        part.read_all().unwrap().as_ref(),
        "café au lait=".as_bytes()
    );
    let bad: [(&[u8], &str); 2] = [(b"bad=ZZ\x01x", "bad=ZZ"), (b"bad\x01x", "bad")];
    for (data, kept) in bad {
        let mut body = b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n".to_vec();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n--b--\r\n");
        let mut reader = Reader::new(Bytes::from(body), b"b");
        let part = reader.next_part().unwrap().unwrap();
        assert_eq!(part.data().as_ref(), kept.as_bytes());
        assert_eq!(
            part.read_all().unwrap_err().to_string(),
            "quotedprintable: invalid unescaped byte 0x01 in body"
        );
        assert!(reader.next_part().unwrap().is_none());
    }
}

// Not upstream's: Go's FormName is empty for a disposition other than
// form-data or one that doesn't parse, and FileName reads RFC 2231 names;
// ReadForm skips a part without a name.
#[test]
fn names_parts() {
    let body = b"--b\r\nContent-Disposition: attachment; name=\"a\"; filename=\"f.txt\"\r\n\r\nv\r\n--b\r\nContent-Disposition: form-data; name=\"a\"; filename*=utf-8''caf%C3%A9.png\r\n\r\nw\r\n--b\r\nContent-Disposition: form-data; name=\"a\"; bad\r\n\r\nw\r\n--b\r\n\r\nnoname\r\n--b--\r\n";
    assert_eq!(
        parts(body, "b"),
        ["|f.txt|v", "a|café.png|w", "||w", "||noname", "EOF"]
    );
    let form = form(body, "b", MAX_FORM_MEMORY).unwrap();
    assert_eq!(form.values().count(), 0);
    let files: Vec<(&str, &str)> = form
        .files()
        .flat_map(|(name, files)| files.iter().map(move |f| (name, f.filename.as_str())))
        .collect();
    assert_eq!(files, [("a", "café.png")]);
}

// Not upstream's: ReadForm fails on a part whose data doesn't end in a
// boundary, a field or a file.
#[test]
fn read_form_fails_on_truncated_data() {
    for disposition in ["name=\"a\"", "name=\"a\"; filename=\"f\""] {
        let body = format!("--b\r\nContent-Disposition: form-data; {disposition}\r\n\r\nhello");
        assert_eq!(
            form(body.as_bytes(), "b", MAX_FORM_MEMORY).unwrap_err(),
            Error::UnexpectedEof
        );
    }
    let body = b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n";
    assert_eq!(
        form(body, "b", MAX_FORM_MEMORY).unwrap().values().count(),
        0
    );
}

/// A form of `count` fields named `a`.
fn fields(count: usize) -> Vec<u8> {
    let mut body = Vec::new();
    for _ in 0..count {
        body.extend_from_slice(b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nv\r\n");
    }
    body.extend_from_slice(b"--b--\r\n");
    body
}

// Not upstream's: Go's ReadForm takes at most 1000 parts.
#[test]
fn read_form_limits_parts() {
    let form_1000 = form(&fields(1000), "b", MAX_FORM_MEMORY).unwrap();
    assert_eq!(form_1000.values().next().unwrap().1.len(), 1000);
    assert_eq!(
        form(&fields(1001), "b", MAX_FORM_MEMORY).unwrap_err(),
        Error::TooLarge
    );
}

// Not upstream's: ReadForm counts a field's name, 200 bytes for it and
// its data against 10 MiB more than the memory limit.
#[test]
fn read_form_limits_values() {
    for (len, ok) in [((10 << 20) - 201, true), ((10 << 20) - 200, false)] {
        let mut body = b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n".to_vec();
        body.resize(body.len() + len, b'v');
        body.extend_from_slice(b"\r\n--b--\r\n");
        let read = form(&body, "b", 0);
        match read {
            Ok(form) => {
                assert!(ok, "{len}");
                assert_eq!(form.value("a").unwrap().len(), len);
            }
            Err(error) => {
                assert!(!ok, "{len}");
                assert_eq!(error, Error::TooLarge);
                assert_eq!(error.to_string(), "multipart: message too large");
            }
        }
    }
}

// Not upstream's: a file over the memory limit is kept (Go keeps it on
// disk), and a header over the limit fails.
#[test]
fn read_form_limits_files_and_headers() {
    let mut body =
        b"--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"f\"\r\n\r\n".to_vec();
    body.resize(body.len() + 100, b'v');
    body.extend_from_slice(b"\r\n--b--\r\n");
    let form_10 = form(&body, "b", 10).unwrap();
    assert_eq!(form_10.files_of("f")[0].data.len(), 100);

    let mut body = b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\nX-Big: ".to_vec();
    body.resize(body.len() + (11 << 20), b'h');
    body.extend_from_slice(b"\r\n\r\nv\r\n--b--\r\n");
    assert_eq!(form(&body, "b", 0).unwrap_err(), Error::TooLarge);
    assert_eq!(parts(&body, "b"), ["error multipart: message too large"]);
}

// Not upstream's: the Debug of a form, a part, a file and a header shows
// names and sizes, never data.
#[test]
fn debug_hides_data() {
    let body = format!(
        "--b\r\nContent-Disposition: form-data; name=\"prompt\"\r\nX-Note: {SECRET}\r\n\r\n{SECRET}\r\n--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\n\r\n{SECRET}\r\n--b--\r\n"
    );
    let read = form(body.as_bytes(), "b", MAX_FORM_MEMORY).unwrap();
    let debug = format!("{read:?}");
    assert!(!debug.contains(SECRET), "{debug}");
    assert!(
        debug.contains("prompt") && debug.contains("a.png"),
        "{debug}"
    );
    let mut reader = Reader::new(Bytes::from(body), b"b");
    let part = reader.next_part().unwrap().unwrap();
    let debug = format!("{part:?} {reader:?}");
    assert!(!debug.contains(SECRET), "{debug}");
    let mut writer = Writer::with_boundary("b").unwrap();
    writer.write_field("prompt", SECRET.as_bytes());
    assert!(!format!("{writer:?}").contains(SECRET));
}

// Not upstream's: Go's Writer, field by field and file by file.
#[test]
fn writes_a_form() {
    let mut writer = Writer::with_boundary("abc").unwrap();
    writer.write_field("prompt", b"a \"cat\"\r\n");
    writer.write_file("image[]", "dir/x.png", b"\x89PNG");
    assert_eq!(
        writer.form_data_content_type(),
        "multipart/form-data; boundary=abc"
    );
    let body = writer.finish();
    assert_eq!(
        body,
        b"--abc\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\na \"cat\"\r\n\r\n--abc\r\nContent-Disposition: form-data; name=\"image[]\"; filename=\"dir/x.png\"\r\nContent-Type: application/octet-stream\r\n\r\n\x89PNG\r\n--abc--\r\n"
    );
    let read = form(&body, "abc", MAX_FORM_MEMORY).unwrap();
    assert_eq!(read.value("prompt").unwrap().as_ref(), b"a \"cat\"\r\n");
    assert_eq!(read.files_of("image[]")[0].filename, "x.png");
    assert_eq!(read.files_of("image[]")[0].data.as_ref(), b"\x89PNG");

    let writer = Writer::with_boundary("a b").unwrap();
    assert_eq!(
        writer.form_data_content_type(),
        "multipart/form-data; boundary=\"a b\""
    );
    assert_eq!(writer.finish(), b"\r\n--a b--\r\n");
}

// Not upstream's: Go's SetBoundary and its random boundary.
#[test]
fn boundaries() {
    let long = "a".repeat(71);
    let longest = "a".repeat(70);
    let cases = [
        ("", Some("mime: invalid boundary length")),
        (long.as_str(), Some("mime: invalid boundary length")),
        (longest.as_str(), None),
        ("a ", Some("mime: invalid boundary character")),
        ("a b", None),
        ("a\"b", Some("mime: invalid boundary character")),
        ("a;b", Some("mime: invalid boundary character")),
        ("'()+_,-./:=?", None),
    ];
    for (boundary, want) in cases {
        let got = Writer::with_boundary(boundary).err().map(|e| e.to_string());
        assert_eq!(got.as_deref(), want, "{boundary:?}");
    }
    let first = Writer::new();
    let second = Writer::new();
    assert_eq!(first.boundary().len(), 60);
    assert!(first.boundary().bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(first.boundary(), second.boundary());
}

// Not upstream's: Go's FormatMediaType, FileContentDisposition and
// ParseMediaType.
#[test]
fn media_types() {
    assert_eq!(
        format_media_type(
            "form-data",
            &[("name", b"a b"), ("filename", b"\"x\\\".png")]
        ),
        "form-data; filename=\"\\\"x\\\\\\\".png\"; name=\"a b\""
    );
    assert_eq!(
        format_media_type(
            "form-data",
            &[("name", b"image"), ("filename", "café 1%.png".as_bytes())]
        ),
        "form-data; filename*=utf-8''caf%C3%A9%201%25.png; name=image"
    );
    assert_eq!(format_media_type("Image/PNG", &[]), "image/png");
    assert_eq!(format_media_type("form-data", &[("bad name", b"x")]), "");
    assert_eq!(format_media_type("a/b/c", &[]), "");
    assert_eq!(
        file_content_disposition("a\"b", "c\\d\ne"),
        "form-data; name=\"a\\\"b\"; filename=\"c\\\\d%0Ae\""
    );

    let (kind, params) =
        parse_media_type(b"multipart/form-data; boundary=\"a b\"; charset=UTF-8").unwrap();
    assert_eq!(kind, "multipart/form-data");
    assert_eq!(
        params.get("boundary").map(Vec::as_slice),
        Some(b"a b".as_slice())
    );
    assert_eq!(
        params.get("charset").map(Vec::as_slice),
        Some(b"UTF-8".as_slice())
    );
    let error = parse_media_type(b"multipart/form-data; boundary").unwrap_err();
    assert_eq!(error.to_string(), "mime: invalid media parameter");
    assert_eq!(error.media_type(), "multipart/form-data");
}

// Not upstream's: Go's filepath.Base, splitting at `\` too.
#[test]
fn base_names() {
    let cases: [(&[u8], &[u8]); 6] = [
        (b"", b"."),
        (b"a.png", b"a.png"),
        (b"../dir/a.png", b"a.png"),
        (b"C:\\dir\\a.png", b"a.png"),
        (b"dir/", b"dir"),
        (b"//", b"\\"),
    ];
    for (path, want) in cases {
        assert_eq!(base_name(path), want, "{}", lossy(path));
    }
}
