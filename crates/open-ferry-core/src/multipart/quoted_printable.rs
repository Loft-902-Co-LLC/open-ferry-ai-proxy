// Ported from Go's mime/quotedprintable/reader.go (Reader.Read, fromHex,
// readHexByte, isQPDiscardWhitespace) and bufio (Reader.ReadSlice)
// (go1.26, BSD-3-Clause, see licenses/Go-LICENSE).
// https://github.com/golang/go

//! The quoted-printable decoding of a part sent with
//! `Content-Transfer-Encoding: quoted-printable`.
//!
//! Deviations from Go: none, but that a line is read from the whole of the
//! part's data at once, so a line without a newline that is exactly 4096
//! bytes long, the size of Go's buffer, ends the data rather than failing
//! with `bufio: buffer full`, whichever Go's reads would have given.

use open_ferry_translate::go::quote_bytes;

use super::Error;

/// The size of Go's `bufio.Reader`, the longest line it reads at once.
const BUFFER: usize = 4096;

/// `raw` decoded, with the error that ended the decoding: `end`, the error
/// the raw data ended with (`None` for its end), or a decoding error.
pub(super) fn decode(raw: &[u8], end: Option<Error>) -> (Vec<u8>, Option<Error>) {
    let mut out = Vec::with_capacity(raw.len());
    let mut pos = 0;
    loop {
        // `ReadSlice('\n')` on a 4096-byte buffer.
        let rest = raw.get(pos..).unwrap_or_default();
        let window = rest.get(..BUFFER).unwrap_or(rest);
        let (whole, mut stop): (&[u8], Option<Option<Error>>) =
            match window.iter().position(|&b| b == b'\n') {
                Some(i) => (rest.get(..=i).unwrap_or(rest), None),
                None if rest.len() > BUFFER => (
                    window,
                    Some(Some(Error::Other("bufio: buffer full".to_owned()))),
                ),
                None => (rest, Some(end.clone())),
            };
        pos += whole.len();

        let has_lf = whole.ends_with(b"\n");
        let has_crlf = whole.ends_with(b"\r\n");
        let kept = whole
            .iter()
            .rposition(|&b| !matches!(b, b'\n' | b'\r' | b' ' | b'\t'))
            .map_or(0, |last| last + 1);
        let mut line: Vec<u8> = whole.get(..kept).unwrap_or_default().to_vec();
        if line.last() == Some(&b'=') {
            let after = whole.get(kept..).unwrap_or_default();
            let right = trim_left_lwsp(after);
            line.pop();
            let at_end = matches!(stop, Some(None));
            if !right.starts_with(b"\n")
                && !right.starts_with(b"\r\n")
                && !(right.is_empty() && !line.is_empty() && at_end)
            {
                stop = Some(Some(Error::Other(format!(
                    "quotedprintable: invalid bytes after =: {}",
                    quote_bytes(right)
                ))));
            }
        } else if has_lf {
            if has_crlf {
                line.push(b'\r');
            }
            line.push(b'\n');
        }

        if let Some(error) = decode_line(&line, &mut out) {
            return (out, Some(error));
        }
        if let Some(stop) = stop {
            return (out, stop);
        }
    }
}

/// Decodes one line, as Go's `Read` takes its bytes, into `out`; the error
/// that stops it, if one does.
fn decode_line(mut line: &[u8], out: &mut Vec<u8>) -> Option<Error> {
    while let Some((&b, rest)) = line.split_first() {
        match b {
            b'=' => match hex_byte(rest) {
                Ok(byte) => {
                    out.push(byte);
                    line = rest.get(2..).unwrap_or_default();
                    continue;
                }
                Err(error) => {
                    if !matches!(rest.first(), None | Some(b'\r' | b'\n')) {
                        out.push(b'=');
                        line = rest;
                        continue;
                    }
                    return Some(error);
                }
            },
            b'\t' | b'\r' | b'\n' | 0x80..=0xff => {}
            _ if !(b' '..=b'~').contains(&b) => {
                return Some(Error::Other(format!(
                    "quotedprintable: invalid unescaped byte 0x{b:02x} in body"
                )));
            }
            _ => {}
        }
        out.push(b);
        line = rest;
    }
    None
}

/// Go's `readHexByte`: the byte two hex digits at the start of `v` stand
/// for.
fn hex_byte(v: &[u8]) -> Result<u8, Error> {
    let [high, low, ..] = v else {
        return Err(Error::UnexpectedEof);
    };
    Ok((from_hex(*high)? << 4) | from_hex(*low)?)
}

/// Go's `fromHex`: an upper- or lower-case hex digit's value.
fn from_hex(b: u8) -> Result<u8, Error> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(Error::Other(format!(
            "quotedprintable: invalid hex byte 0x{b:02x}"
        ))),
    }
}

/// `bytes` past any spaces and tabs at its start.
fn trim_left_lwsp(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|&b| b != b' ' && b != b'\t')
        .unwrap_or(bytes.len());
    bytes.get(start..).unwrap_or_default()
}
