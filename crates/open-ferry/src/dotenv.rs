// Ported from joho/godotenv v1.5.1 parser.go (parseBytes, getStatementStart,
// locateKeyName, extractVarValue, expandEscapes, indexOfNonSpaceChar,
// hasQuotePrefix, isSpace, isLineEnd, expandVariables) and godotenv.go
// (Load, loadFile, readFile, Parse) (MIT), as CLIProxyAPI's
// cmd/server/main.go loads `.env` with it (v8.0.20, MIT).
// https://github.com/joho/godotenv
// https://github.com/router-for-me/CLIProxyAPI

//! The `.env` file in the working directory, which upstream loads into the
//! environment at start with godotenv.
//!
//! [`read`] parses a file as godotenv v1.5.1 does:
//! - a statement sets a variable with `NAME=value` or `NAME: value`, after
//!   an optional `export`; a name holds letters, digits, `_` and `.`. Blank
//!   lines and lines that start with `#` are skipped;
//! - an unquoted value runs to the end of its line, is cut at its last `#`
//!   that follows a space, and is trimmed of spaces;
//! - a value in single quotes is kept as written, across lines too;
//! - a value in double quotes may span lines too; in it, `\n` is a newline,
//!   `\r` a carriage return, and any other backslash but one before `$` is
//!   dropped from before the character it escapes;
//! - unquoted and double-quoted values expand `$NAME` and `${NAME}`, for a
//!   name of capitals, digits and `_`, to the value the file gave the name
//!   above, or to nothing; `\$` is a `$`.
//!
//! A file that doesn't parse sets nothing. [`missing`] picks the variables
//! to set, those the environment doesn't have, so a variable set in the
//! environment wins over the file. `main` sets them before it starts any
//! thread.
//!
//! Deviations from upstream:
//! - A variable the environment has is kept whatever the case of its name.
//!   godotenv compares names case-sensitively, so on Windows, whose names
//!   ignore case, a file's `path` replaced the environment's `Path`.
//! - A UTF-8 byte order mark at the start of the file is skipped. godotenv
//!   fails on it, so a file that Windows PowerShell 5 wrote as UTF-8 didn't
//!   load.
//! - An error names its line and never quotes the file. godotenv's quote the
//!   rest of the file, or the unterminated value, and upstream logs them.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::Path;

/// A `.env` file's variables by name, as godotenv's `Parse` gives them.
/// Names and values are bytes, since a quoted value keeps the bytes that
/// aren't UTF-8.
pub type Vars = BTreeMap<Vec<u8>, Vec<u8>>;

/// The byte order mark UTF-8 text may start with.
const BOM: &[u8] = b"\xef\xbb\xbf";

const BACKSLASH: u8 = b'\\';

/// Why a `.env` file wasn't loaded.
#[derive(Debug)]
pub enum Error {
    /// The file couldn't be read.
    Read(io::Error),
    /// The file doesn't parse at `line`, counted from 1.
    Parse { line: usize, problem: Problem },
}

impl Error {
    /// Whether there is no file, which upstream doesn't warn about.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::Read(error) if error.kind() == io::ErrorKind::NotFound)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Read(error) => error.fmt(f),
            Error::Parse { line, problem } => write!(f, "{problem} on line {line}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Read(error) => Some(error),
            Error::Parse { .. } => None,
        }
    }
}

/// What godotenv's parser stopped at. None of them holds the file's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// A byte that can't be in a name, where a name was being read. It is
    /// shown as the Latin-1 character godotenv takes it for.
    UnexpectedCharacter(u8),
    /// A quoted value without its closing quote.
    UnterminatedQuote,
    /// `export` with nothing after it at the end of the file.
    ZeroLengthString,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::UnexpectedCharacter(byte) => write!(
                f,
                "unexpected character {:?} in variable name",
                char::from(*byte)
            ),
            Problem::UnterminatedQuote => f.write_str("unterminated quoted value"),
            Problem::ZeroLengthString => f.write_str("zero length string"),
        }
    }
}

/// Reads and parses the file at `path`, as godotenv's `readFile` does.
pub fn read(path: &Path) -> Result<Vars, Error> {
    let src = std::fs::read(path).map_err(Error::Read)?;
    parse(&src)
}

/// The variables of `vars` to set in an environment of which `is_set` tells
/// the names that are set, as godotenv's `loadFile` picks them. Those that
/// Go's `os.Setenv` refuses, and godotenv then skips, are left out: an empty
/// name, a name with `=` or NUL, and a value with NUL, which
/// `std::env::set_var` would panic on.
///
/// On Unix the names and values are the file's bytes, as Go sets them. On
/// Windows, whose environment is UTF-16, each byte that isn't UTF-8 becomes
/// U+FFFD, as Go converts it.
pub fn missing(vars: Vars, is_set: impl Fn(&OsStr) -> bool) -> Vec<(OsString, OsString)> {
    vars.into_iter()
        .filter(|(key, value)| {
            !key.is_empty() && !key.contains(&b'=') && !key.contains(&0) && !value.contains(&0)
        })
        .map(|(key, value)| (os_string(key), os_string(value)))
        .filter(|(key, _)| !is_set(key))
        .collect()
}

/// Whether the process's environment has `key`. On Windows, names ignore
/// case.
pub fn in_environment(key: &OsStr) -> bool {
    std::env::var_os(key).is_some()
}

#[cfg(unix)]
fn os_string(bytes: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes)
}

#[cfg(not(unix))]
fn os_string(bytes: Vec<u8>) -> OsString {
    OsString::from(go_runes(&bytes))
}

/// Where a statement stopped the parser: the rest of the file from there,
/// and why.
type Stop<'a> = (&'a [u8], Problem);

/// Parses a file's bytes, as godotenv's `parseBytes` does.
pub(crate) fn parse(src: &[u8]) -> Result<Vars, Error> {
    let src = replace_crlf(src.strip_prefix(BOM).unwrap_or(src));
    let mut out = Vars::new();
    let mut cutset: &[u8] = &src;
    while let Some(statement) = get_statement_start(cutset) {
        let (key, left) = locate_key_name(statement).map_err(|stop| parse_error(&src, stop))?;
        let (value, left) =
            extract_var_value(left, &out).map_err(|stop| parse_error(&src, stop))?;
        out.insert(key, value);
        cutset = left;
    }
    Ok(out)
}

/// The error for `stop`, with the line it is on in `src`.
fn parse_error(src: &[u8], (at, problem): Stop<'_>) -> Error {
    // `at` is the end of `src`, from where the parser stopped.
    let offset = src.len().saturating_sub(at.len());
    let newlines = src
        .get(..offset)
        .unwrap_or_default()
        .iter()
        .filter(|&&byte| byte == b'\n');
    Error::Parse {
        line: newlines.count() + 1,
        problem,
    }
}

/// `bytes.Replace(src, "\r\n", "\n", -1)`.
fn replace_crlf(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut bytes = src.iter().peekable();
    while let Some(&byte) = bytes.next() {
        if byte == b'\r' && bytes.peek() == Some(&&b'\n') {
            continue;
        }
        out.push(byte);
    }
    out
}

/// `getStatementStart`: `src` from its next statement, past white space and
/// comments, or `None` at the end.
fn get_statement_start(mut src: &[u8]) -> Option<&[u8]> {
    loop {
        src = src.get(index_of_non_space_char(src)?..)?;
        if src.first() != Some(&b'#') {
            return Some(src);
        }
        // A comment: go on from the end of its line.
        src = src.get(src.iter().position(|&byte| byte == b'\n')?..)?;
    }
}

/// `locateKeyName`: the name a statement sets, and what follows its `=` or
/// `:`. godotenv reads a name a byte at a time, each byte the Latin-1
/// character of its value, so Latin-1's letters and digits may be in a name
/// too.
fn locate_key_name(src: &[u8]) -> Result<(Vec<u8>, &[u8]), Stop<'_>> {
    let mut src = trim_left_space(src);
    if let Some(trimmed) = src.strip_prefix(b"export")
        && decode_rune(trimmed).is_some_and(|(rune, _)| rune.is_some_and(is_space))
    {
        src = trim_left_space(trimmed);
    }

    // A statement without `=` or `:` sets the empty name, as in godotenv,
    // whose `os.Setenv` then refuses it (see `missing`).
    let mut key: &[u8] = &[];
    let mut offset = 0;
    for (index, &byte) in src.iter().enumerate() {
        let latin1 = char::from(byte);
        if is_space(latin1) {
            continue;
        }
        match byte {
            b'=' | b':' => {
                key = src.get(..index).unwrap_or_default();
                offset = index + 1;
                break;
            }
            b'_' | b'.' => {}
            // Go's `unicode.IsLetter` or `unicode.IsNumber`, which agree with
            // `is_alphanumeric` on Latin-1.
            _ if latin1.is_alphanumeric() => {}
            _ => {
                let at = src.get(index..).unwrap_or_default();
                return Err((at, Problem::UnexpectedCharacter(byte)));
            }
        }
    }
    if src.is_empty() {
        return Err((src, Problem::ZeroLengthString));
    }

    let key = trim_right_white_space(key).to_vec();
    Ok((key, trim_left_space(src.get(offset..).unwrap_or_default())))
}

/// `extractVarValue`: the value a statement sets, given `vars`, the
/// variables before it, and what follows the value.
fn extract_var_value<'a>(src: &'a [u8], vars: &Vars) -> Result<(Vec<u8>, &'a [u8]), Stop<'a>> {
    let Some(quote) = has_quote_prefix(src) else {
        // An unquoted value: up to the end of the line.
        let end_of_line = src
            .iter()
            .position(|&byte| is_line_end(byte))
            .unwrap_or(src.len());
        let rest = src.get(end_of_line..).unwrap_or_default();
        // godotenv reads the line as runes, a byte that isn't UTF-8 as U+FFFD,
        // and cuts it at its last `#` after a space (`asdasd # some comment`).
        let line: Vec<char> = go_runes(src.get(..end_of_line).unwrap_or_default())
            .chars()
            .collect();
        let end_of_var = line
            .windows(2)
            .rposition(|pair| matches!(pair, &[before, '#'] if is_space(before)))
            .map_or(line.len(), |position| position + 1);
        let value: String = line.get(..end_of_var).unwrap_or_default().iter().collect();
        return Ok((
            expand_variables(value.trim_matches(is_space).as_bytes(), vars),
            rest,
        ));
    };

    // A quoted value: up to the first quote that doesn't follow a backslash.
    let close = src
        .windows(2)
        .position(|pair| matches!(pair, &[before, byte] if byte == quote && before != BACKSLASH));
    let Some(index) = close.map(|position| position + 1) else {
        return Err((src, Problem::UnterminatedQuote));
    };
    let value = trim_byte(src.get(..index).unwrap_or_default(), quote);
    let value = if quote == b'"' {
        // Escapes, then variables, in double quotes only.
        expand_variables(&expand_escapes(value), vars)
    } else {
        value.to_vec()
    };
    Ok((value, src.get(index + 1..).unwrap_or_default()))
}

/// `expandEscapes`, for a value in double quotes. godotenv replaces Go's
/// `\\.`, a backslash and any character but a newline, turning `\n` and `\r`
/// into a newline and a carriage return and keeping the others, then
/// replaces `\\([^$])`, a backslash and any character but `$`, with the
/// character. Each match ends where the next may start, so `\\n` is a
/// backslash and an `n`.
fn expand_escapes(src: &[u8]) -> Vec<u8> {
    // The character a backslash escapes may be longer than a byte, but its
    // other bytes aren't backslashes, so it may be taken a byte at a time.
    let mut escaped = Vec::with_capacity(src.len());
    let mut index = 0;
    while let Some(&byte) = src.get(index) {
        match (byte, src.get(index + 1)) {
            (BACKSLASH, Some(&b'n')) => escaped.push(b'\n'),
            (BACKSLASH, Some(&b'r')) => escaped.push(b'\r'),
            (BACKSLASH, Some(&next)) if next != b'\n' => escaped.extend_from_slice(&[byte, next]),
            _ => {
                escaped.push(byte);
                index += 1;
                continue;
            }
        }
        index += 2;
    }

    let mut out = Vec::with_capacity(escaped.len());
    let mut index = 0;
    while let Some(&byte) = escaped.get(index) {
        match (byte, escaped.get(index + 1)) {
            (BACKSLASH, Some(&next)) if next != b'$' => {
                out.push(next);
                index += 2;
            }
            _ => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

/// `expandVariables`. godotenv replaces each match of
/// `(\\)?(\$)(\()?\{?([A-Z0-9_]+)?\}?` with:
/// - the match less its backslash, when it starts with one;
/// - else the value of the name in it in `vars`, or nothing, when it holds a
///   name;
/// - else the match as it is.
///
/// So `$(NAME` is the name's value too, and a lone `$` stays.
fn expand_variables(src: &[u8], vars: &Vars) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut index = 0;
    while let Some(&byte) = src.get(index) {
        let escaped = byte == BACKSLASH && src.get(index + 1) == Some(&b'$');
        if !escaped && byte != b'$' {
            out.push(byte);
            index += 1;
            continue;
        }
        // From the `$`, each part the expression may match, if it is there.
        let dollar = if escaped { index + 1 } else { index };
        let mut end = dollar + 1;
        if src.get(end) == Some(&b'(') {
            end += 1;
        }
        if src.get(end) == Some(&b'{') {
            end += 1;
        }
        let name_start = end;
        while src
            .get(end)
            .is_some_and(|byte| matches!(byte, b'A'..=b'Z' | b'0'..=b'9' | b'_'))
        {
            end += 1;
        }
        let name = src.get(name_start..end).unwrap_or_default();
        if src.get(end) == Some(&b'}') {
            end += 1;
        }

        if escaped || name.is_empty() {
            out.extend_from_slice(src.get(dollar..end).unwrap_or_default());
        } else if let Some(value) = vars.get(name) {
            out.extend_from_slice(value);
        }
        index = end;
    }
    out
}

/// `indexOfNonSpaceChar`: where the first character that isn't Unicode white
/// space starts, as Go's `unicode.IsSpace` tells it.
fn index_of_non_space_char(src: &[u8]) -> Option<usize> {
    let mut index = 0;
    while let Some((rune, width)) = src.get(index..).and_then(decode_rune) {
        if !rune.is_some_and(char::is_whitespace) {
            return Some(index);
        }
        index += width;
    }
    None
}

/// `hasQuotePrefix`: the quote a value starts with, if it does.
fn has_quote_prefix(src: &[u8]) -> Option<u8> {
    src.first()
        .copied()
        .filter(|&byte| byte == b'"' || byte == b'\'')
}

/// godotenv's `isSpace`: the white space it trims, which has no newline.
fn is_space(rune: char) -> bool {
    matches!(
        rune,
        '\t' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{85}' | '\u{a0}'
    )
}

/// `isLineEnd`.
fn is_line_end(byte: u8) -> bool {
    byte == b'\n' || byte == b'\r'
}

/// `bytes.TrimLeftFunc(src, isSpace)`.
fn trim_left_space(mut src: &[u8]) -> &[u8] {
    while let Some((Some(rune), width)) = decode_rune(src) {
        if !is_space(rune) {
            break;
        }
        src = src.get(width..).unwrap_or_default();
    }
    src
}

/// `strings.TrimRightFunc(src, unicode.IsSpace)`.
fn trim_right_white_space(mut src: &[u8]) -> &[u8] {
    while let Some((Some(rune), width)) = decode_last_rune(src) {
        if !rune.is_whitespace() {
            break;
        }
        src = src
            .get(..src.len().saturating_sub(width))
            .unwrap_or_default();
    }
    src
}

/// `bytes.TrimLeftFunc(bytes.TrimRightFunc(src, isCharFunc(quote)), ...)`.
fn trim_byte(mut src: &[u8], quote: u8) -> &[u8] {
    while let [rest @ .., last] = src {
        if *last != quote {
            break;
        }
        src = rest;
    }
    while let [first, rest @ ..] = src {
        if *first != quote {
            break;
        }
        src = rest;
    }
    src
}

/// `string([]rune(string(bytes)))` in Go: each byte that isn't part of valid
/// UTF-8 becomes U+FFFD.
fn go_runes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        out.extend(chunk.invalid().iter().map(|_| char::REPLACEMENT_CHARACTER));
    }
    out
}

/// The first character of `src` and its length in bytes, as Go's
/// `utf8.DecodeRune` reads it: a byte that doesn't start valid UTF-8 is
/// `None`, one byte long. `None` for an empty `src`.
fn decode_rune(src: &[u8]) -> Option<(Option<char>, usize)> {
    let width = match *src.first()? {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Some((None, 1)),
    };
    let rune = src
        .get(..width)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| text.chars().next());
    Some(match rune {
        Some(rune) => (Some(rune), width),
        None => (None, 1),
    })
}

/// The last character of `src` and its length in bytes, as Go's
/// `utf8.DecodeLastRune` reads it. `None` for an empty `src`.
fn decode_last_rune(src: &[u8]) -> Option<(Option<char>, usize)> {
    if src.is_empty() {
        return None;
    }
    for width in 1..=src.len().min(4) {
        let tail = src
            .get(src.len().saturating_sub(width)..)
            .unwrap_or_default();
        match decode_rune(tail) {
            Some((Some(rune), length)) if length == width => return Some((Some(rune), width)),
            _ => {}
        }
    }
    Some((None, 1))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};

    use super::*;

    // godotenv's fixtures/comments.env.
    const COMMENTS: &str = concat!(
        "# Full line comment\n",
        "foo=bar # baz\n",
        "bar=foo#baz\n",
        "baz=\"foo\"#bar\n",
    );

    // godotenv's fixtures/equals.env.
    const EQUALS: &str = "export OPTION_A='postgres://localhost:5432/database?sslmode=disable'\n";

    // godotenv's fixtures/exported.env.
    const EXPORTED: &str = "export OPTION_A=2\nexport OPTION_B='\\n'\n";

    // godotenv's fixtures/invalid1.env.
    const INVALID1: &str = "INVALID LINE\nfoo=bar\n";

    // godotenv's fixtures/plain.env, which doesn't end with a newline.
    const PLAIN: &str = concat!(
        "OPTION_A=1\n",
        "OPTION_B=2\n",
        "OPTION_C= 3\n",
        "OPTION_D =4\n",
        "OPTION_E = 5\n",
        "OPTION_F = \n",
        "OPTION_G=\n",
        "OPTION_H=1 2",
    );

    // godotenv's fixtures/quoted.env.
    const QUOTED: &str = r#"OPTION_A='1'
OPTION_B='2'
OPTION_C=''
OPTION_D='\n'
OPTION_E="1"
OPTION_F="2"
OPTION_G=""
OPTION_H="\n"
OPTION_I = "echo 'asd'"
OPTION_J='line 1
line 2'
OPTION_K='line one
this is \'quoted\'
one more line'
OPTION_L="line 1
line 2"
OPTION_M="line one
this is \"quoted\"
one more line"
"#;

    // godotenv's fixtures/substitutions.env.
    const SUBSTITUTIONS: &str = concat!(
        "OPTION_A=1\n",
        "OPTION_B=${OPTION_A}\n",
        "OPTION_C=$OPTION_B\n",
        "OPTION_D=${OPTION_A}${OPTION_B}\n",
        "OPTION_E=${OPTION_NOT_DEFINED}\n",
    );

    fn vars(pairs: &[(&str, &str)]) -> Vars {
        pairs
            .iter()
            .map(|(key, value)| (key.as_bytes().to_vec(), value.as_bytes().to_vec()))
            .collect()
    }

    /// The value of `key`, or `""`, as a Go map gives it.
    fn get<'a>(vars: &'a Vars, key: &str) -> &'a [u8] {
        vars.get(key.as_bytes())
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// godotenv's `parseAndCompare`.
    #[track_caller]
    fn parse_and_compare(raw_env_line: &str, expected_key: &str, expected_value: &str) {
        let result = parse(raw_env_line.as_bytes())
            .unwrap_or_else(|error| panic!("{raw_env_line:?} errored: {error}"));
        assert_eq!(
            result.get(expected_key.as_bytes()).map(Vec::as_slice),
            Some(expected_value.as_bytes()),
            "{raw_env_line:?}"
        );
    }

    /// godotenv's `loadEnvAndCompareValues`, loading into an environment that
    /// holds only `presets` rather than into the process's.
    #[track_caller]
    fn load_env_and_compare_values(
        src: &str,
        expected_values: &[(&str, &str)],
        presets: &[(&str, &str)],
    ) {
        let vars = parse(src.as_bytes()).unwrap_or_else(|error| panic!("error loading: {error}"));
        let mut env: BTreeMap<OsString, OsString> = presets
            .iter()
            .map(|&(key, value)| (OsString::from(key), OsString::from(value)))
            .collect();
        let set = missing(vars, |key| env.contains_key(key));
        env.extend(set);
        for &(key, value) in expected_values {
            let env_value = env.get(OsStr::new(key)).cloned().unwrap_or_default();
            assert_eq!(env_value, OsString::from(value), "mismatch for key {key:?}");
        }
    }

    // TestLoadFileNotFound.
    #[test]
    fn load_file_not_found() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error = read(&dir.path().join("somefilethatwillneverexistever.env"))
            .expect_err("file wasn't found but read didn't return an error");
        // Not upstream's: `main` doesn't warn about a missing file.
        assert!(error.is_not_found());
    }

    // TestReadPlainEnv.
    #[test]
    fn read_plain_env() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("plain.env");
        std::fs::write(&path, PLAIN).expect("write the fixture");
        let env_map = read(&path).expect("error reading file");
        let expected_values = vars(&[
            ("OPTION_A", "1"),
            ("OPTION_B", "2"),
            ("OPTION_C", "3"),
            ("OPTION_D", "4"),
            ("OPTION_E", "5"),
            ("OPTION_F", ""),
            ("OPTION_G", ""),
            ("OPTION_H", "1 2"),
        ]);
        assert_eq!(env_map, expected_values);
    }

    // TestParse.
    #[test]
    fn parse_text() {
        let env_map = parse(b"ONE=1\nTWO='2'\nTHREE = \"3\"").expect("error parsing env");
        for (key, value) in [("ONE", "1"), ("TWO", "2"), ("THREE", "3")] {
            assert_eq!(get(&env_map, key), value.as_bytes(), "{key}");
        }
    }

    // TestLoadDoesNotOverride.
    #[test]
    fn load_does_not_override() {
        let presets = [("OPTION_A", "do_not_override"), ("OPTION_B", "")];
        load_env_and_compare_values(PLAIN, &presets, &presets);
    }

    // TestLoadPlainEnv.
    #[test]
    fn load_plain_env() {
        let expected_values = [
            ("OPTION_A", "1"),
            ("OPTION_B", "2"),
            ("OPTION_C", "3"),
            ("OPTION_D", "4"),
            ("OPTION_E", "5"),
            ("OPTION_H", "1 2"),
        ];
        load_env_and_compare_values(PLAIN, &expected_values, &[]);
    }

    // TestLoadExportedEnv.
    #[test]
    fn load_exported_env() {
        let expected_values = [("OPTION_A", "2"), ("OPTION_B", "\\n")];
        load_env_and_compare_values(EXPORTED, &expected_values, &[]);
    }

    // TestLoadEqualsEnv.
    #[test]
    fn load_equals_env() {
        let expected_values = [(
            "OPTION_A",
            "postgres://localhost:5432/database?sslmode=disable",
        )];
        load_env_and_compare_values(EQUALS, &expected_values, &[]);
    }

    // TestLoadQuotedEnv.
    #[test]
    fn load_quoted_env() {
        let expected_values = [
            ("OPTION_A", "1"),
            ("OPTION_B", "2"),
            ("OPTION_C", ""),
            ("OPTION_D", "\\n"),
            ("OPTION_E", "1"),
            ("OPTION_F", "2"),
            ("OPTION_G", ""),
            ("OPTION_H", "\n"),
            ("OPTION_I", "echo 'asd'"),
            ("OPTION_J", "line 1\nline 2"),
            ("OPTION_K", "line one\nthis is \\'quoted\\'\none more line"),
            ("OPTION_L", "line 1\nline 2"),
            ("OPTION_M", "line one\nthis is \"quoted\"\none more line"),
        ];
        load_env_and_compare_values(QUOTED, &expected_values, &[]);
    }

    // TestSubstitutions.
    #[test]
    fn substitutions() {
        let expected_values = [
            ("OPTION_A", "1"),
            ("OPTION_B", "1"),
            ("OPTION_C", "1"),
            ("OPTION_D", "11"),
            ("OPTION_E", ""),
        ];
        load_env_and_compare_values(SUBSTITUTIONS, &expected_values, &[]);
    }

    // TestExpanding.
    #[test]
    fn expanding() {
        // A name, the file, and the variables it sets.
        type Case = (
            &'static str,
            &'static str,
            &'static [(&'static str, &'static str)],
        );
        let tests: [Case; 8] = [
            (
                "expands variables found in values",
                "FOO=test\nBAR=$FOO",
                &[("FOO", "test"), ("BAR", "test")],
            ),
            (
                "parses variables wrapped in brackets",
                "FOO=test\nBAR=${FOO}bar",
                &[("FOO", "test"), ("BAR", "testbar")],
            ),
            (
                "expands undefined variables to an empty string",
                "BAR=$FOO",
                &[("BAR", "")],
            ),
            (
                "expands variables in double quoted strings",
                "FOO=test\nBAR=\"quote $FOO\"",
                &[("FOO", "test"), ("BAR", "quote test")],
            ),
            (
                "does not expand variables in single quoted strings",
                "BAR='quote $FOO'",
                &[("BAR", "quote $FOO")],
            ),
            (
                "does not expand escaped variables",
                r#"FOO="foo\$BAR""#,
                &[("FOO", "foo$BAR")],
            ),
            (
                "does not expand escaped variables",
                r#"FOO="foo\${BAR}""#,
                &[("FOO", "foo${BAR}")],
            ),
            (
                "does not expand escaped variables",
                "FOO=test\nBAR=\"foo\\${FOO} ${FOO}\"",
                &[("FOO", "test"), ("BAR", "foo${FOO} test")],
            ),
        ];
        for (name, input, expected) in tests {
            let env = parse(input.as_bytes()).unwrap_or_else(|error| panic!("{name}: {error}"));
            for &(key, value) in expected {
                assert_eq!(get(&env, key), value.as_bytes(), "{name}: {key}");
            }
        }
    }

    // TestVariableStringValueSeparator.
    #[test]
    fn variable_string_value_separator() {
        let input = "TEST_URLS=\"stratum+tcp://stratum.antpool.com:3333\nstratum+tcp://stratum.antpool.com:443\"";
        let want = vars(&[(
            "TEST_URLS",
            "stratum+tcp://stratum.antpool.com:3333\nstratum+tcp://stratum.antpool.com:443",
        )]);
        assert_eq!(parse(input.as_bytes()).expect("parses"), want);
    }

    // TestActualEnvVarsAreLeftAlone.
    #[test]
    fn actual_env_vars_are_left_alone() {
        load_env_and_compare_values(
            PLAIN,
            &[("OPTION_A", "actualenv")],
            &[("OPTION_A", "actualenv")],
        );
    }

    // TestParsing.
    #[test]
    fn parsing() {
        // unquoted values
        parse_and_compare("FOO=bar", "FOO", "bar");

        // parses values with spaces around equal sign
        parse_and_compare("FOO =bar", "FOO", "bar");
        parse_and_compare("FOO= bar", "FOO", "bar");

        // parses double quoted values
        parse_and_compare(r#"FOO="bar""#, "FOO", "bar");

        // parses single quoted values
        parse_and_compare("FOO='bar'", "FOO", "bar");

        // parses escaped double quotes
        parse_and_compare(r#"FOO="escaped\"bar""#, "FOO", r#"escaped"bar"#);

        // parses single quotes inside double quotes
        parse_and_compare(r#"FOO="'d'""#, "FOO", "'d'");

        // parses yaml style options
        parse_and_compare("OPTION_A: 1", "OPTION_A", "1");

        // parses yaml values with equal signs
        parse_and_compare("OPTION_A: Foo=bar", "OPTION_A", "Foo=bar");

        // parses non-yaml options with colons
        parse_and_compare("OPTION_A=1:B", "OPTION_A", "1:B");

        // parses export keyword
        parse_and_compare("export OPTION_A=2", "OPTION_A", "2");
        parse_and_compare(r"export OPTION_B='\n'", "OPTION_B", "\\n");
        parse_and_compare("export exportFoo=2", "exportFoo", "2");
        parse_and_compare("exportFOO=2", "exportFOO", "2");
        parse_and_compare("export_FOO =2", "export_FOO", "2");
        parse_and_compare("export.FOO= 2", "export.FOO", "2");
        parse_and_compare("export\tOPTION_A=2", "OPTION_A", "2");
        parse_and_compare("  export OPTION_A=2", "OPTION_A", "2");
        parse_and_compare("\texport OPTION_A=2", "OPTION_A", "2");

        // expands newlines in quoted strings
        parse_and_compare(r#"FOO="bar\nbaz""#, "FOO", "bar\nbaz");

        // parses variables with "." in the name
        parse_and_compare("FOO.BAR=foobar", "FOO.BAR", "foobar");

        // parses variables with several "=" in the value
        parse_and_compare("FOO=foobar=", "FOO", "foobar=");

        // strips unquoted values
        parse_and_compare("FOO=bar ", "FOO", "bar");

        // unquoted internal whitespace is preserved
        parse_and_compare("KEY=value value", "KEY", "value value");

        // ignores inline comments
        parse_and_compare("FOO=bar # this is foo", "FOO", "bar");

        // allows # in quoted value
        parse_and_compare(r##"FOO="bar#baz" # comment"##, "FOO", "bar#baz");
        parse_and_compare("FOO='bar#baz' # comment", "FOO", "bar#baz");
        parse_and_compare(r##"FOO="bar#baz#bang" # comment"##, "FOO", "bar#baz#bang");

        // parses # in quoted values
        parse_and_compare(r##"FOO="ba#r""##, "FOO", "ba#r");
        parse_and_compare("FOO='ba#r'", "FOO", "ba#r");

        // newlines and backslashes should be escaped
        parse_and_compare(r#"FOO="bar\n\ b\az""#, "FOO", "bar\n baz");
        parse_and_compare(r#"FOO="bar\\\n\ b\az""#, "FOO", "bar\\\n baz");
        parse_and_compare(r#"FOO="bar\\r\ b\az""#, "FOO", "bar\\r baz");

        parse_and_compare(r#"="value""#, "", "value");

        // unquoted whitespace around keys should be ignored
        parse_and_compare(" KEY =value", "KEY", "value");
        parse_and_compare("   KEY=value", "KEY", "value");
        parse_and_compare("\tKEY=value", "KEY", "value");

        // throws an error if line format is incorrect
        let badly_formatted_line = "lol$wut";
        assert!(
            parse(badly_formatted_line.as_bytes()).is_err(),
            "{badly_formatted_line:?}"
        );
    }

    // TestLinesToIgnore.
    #[test]
    fn lines_to_ignore() {
        let cases = [
            ("Line with nothing but line break", "\n", ""),
            ("Line with nothing but windows-style line break", "\r\n", ""),
            ("Line full of whitespace", "\t\t ", ""),
            ("Comment", "# Comment", ""),
            ("Indented comment", "\t # comment", ""),
            (
                "non-ignored value",
                r"export OPTION_B='\n'",
                r"export OPTION_B='\n'",
            ),
        ];
        for (name, input, want) in cases {
            let got = get_statement_start(input.as_bytes()).unwrap_or_default();
            assert_eq!(got, want.as_bytes(), "{name}");
        }
    }

    // TestErrorReadDirectory.
    #[test]
    fn error_read_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error = read(dir.path()).expect_err("expected an error reading a directory");
        // Not upstream's: `main` warns about it, as upstream does.
        assert!(!error.is_not_found(), "{error}");
    }

    // TestErrorParsing.
    #[test]
    fn error_parsing() {
        let error = parse(INVALID1.as_bytes()).expect_err("expected an error");
        // Not upstream's: the error names the line, and not the text.
        assert_eq!(
            error.to_string(),
            "unexpected character '\\n' in variable name on line 1"
        );
    }

    // TestComments.
    #[test]
    fn comments() {
        let expected_values = [("foo", "bar"), ("bar", "foo#baz"), ("baz", "foo")];
        load_env_and_compare_values(COMMENTS, &expected_values, &[]);
    }

    // TestTrailingNewlines.
    #[test]
    fn trailing_newlines() {
        let cases = [
            (
                "Simple value without trailing newline",
                "KEY=value",
                "KEY",
                "value",
            ),
            (
                "Value with internal whitespace without trailing newline",
                "KEY=value value",
                "KEY",
                "value value",
            ),
            (
                "Value with internal whitespace with trailing newline",
                "KEY=value value\n",
                "KEY",
                "value value",
            ),
            (
                "YAML style - value with internal whitespace without trailing newline",
                "KEY: value value",
                "KEY",
                "value value",
            ),
            (
                "YAML style - value with internal whitespace with trailing newline",
                "KEY: value value\n",
                "KEY",
                "value value",
            ),
        ];
        for (name, input, key, value) in cases {
            let result = parse(input.as_bytes()).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(get(&result, key), value.as_bytes(), "{name}");
        }
    }

    #[track_caller]
    fn assert_parses(input: &[u8], expected: &[(&str, &str)]) {
        let got = parse(input).unwrap_or_else(|error| panic!("{input:?}: {error}"));
        assert_eq!(got, vars(expected), "{:?}", String::from_utf8_lossy(input));
    }

    #[track_caller]
    fn assert_stops(input: &[u8], line: usize, problem: Problem) {
        match parse(input) {
            Err(Error::Parse {
                line: got_line,
                problem: got_problem,
            }) => {
                assert_eq!((got_line, got_problem), (line, problem), "{input:?}");
            }
            other => panic!("{input:?}: {other:?}"),
        }
    }

    // Not upstream's: what a Go probe of godotenv v1.5.1's Unmarshal gave for
    // each of these, the corners its own tests leave out.
    #[test]
    fn parses_as_godotenv_does() {
        assert_parses(b"FOO=a #b #c", &[("FOO", "a #b")]);
        assert_parses(b"BAR=x\nFOO=$(BAR)", &[("BAR", "x"), ("FOO", "x)")]);
        assert_parses(b"BAR=x\nFOO=${BAR", &[("BAR", "x"), ("FOO", "x")]);
        assert_parses(b"bar=x\nFOO=$bar", &[("FOO", "$bar"), ("bar", "x")]);
        assert_parses(b"BAR=x\nFOO=\\$BAR", &[("BAR", "x"), ("FOO", "$BAR")]);
        assert_parses(b"FOO", &[("", "FOO")]);
        assert_parses(b"FOO='a'b", &[("", "b"), ("FOO", "a")]);
        assert_parses(b"FOO=x\n=y", &[("", "y"), ("FOO", "x")]);
        assert_parses(b"A=x\nB=\"$A\\$A\\\\$A\"", &[("A", "x"), ("B", "x$A$A")]);
        assert_parses(b"FOO=\"bar\\\nbaz\"", &[("FOO", "bar\nbaz")]);
        assert_parses(b"# a\n# b\nFOO=1", &[("FOO", "1")]);
        assert_parses(
            b"FOO=1\r\nBAR=2\rBAZ=3",
            &[("BAR", "2"), ("BAZ", "3"), ("FOO", "1")],
        );
        assert_parses(b"FOO = 'x' # c\nBAR=y", &[("BAR", "y"), ("FOO", "x")]);
        assert_parses(b"FOO=\"x\" BAR=2", &[("BAR", "2"), ("FOO", "x")]);
        assert_parses(b"FOO:bar", &[("FOO", "bar")]);
        assert_parses(b"  # comment only", &[]);
        assert_parses(b"#FOO=1", &[]);
        assert_parses(b"export=1", &[("export", "1")]);
        assert_parses(b"FOO=${A}${B}", &[("FOO", "")]);
        assert_parses(b"FOO\tBAR=1", &[("FOO\tBAR", "1")]);
        assert_parses(b"FOO=$", &[("FOO", "$")]);
        assert_parses(b"FOO=${}", &[("FOO", "${}")]);
        assert_parses(b"FOO=\"a\\tb\\$c\\\\d\"", &[("FOO", "atb$c\\d")]);
        assert_parses(b"FOO=\"a\\nb\\rc\\\\d\"", &[("FOO", "a\nb\rc\\d")]);
        assert_parses(b"FOO=\"\\\"x\\\"\"", &[("FOO", "\"x\\")]);
        assert_parses(b"FOO='\\''", &[("FOO", "\\")]);
        assert_parses(b"FOO=a\\nb", &[("FOO", "a\\nb")]);
        assert_parses(b"FOO='a\\nb $X'", &[("FOO", "a\\nb $X")]);
        assert_parses(b"A=1\nA=2\nB=$A", &[("A", "2"), ("B", "2")]);
        assert_parses(b"X=1\nFOO=\"${X}y $X_z\"", &[("FOO", "1y z"), ("X", "1")]);
        assert_parses(b"FOO=\"$BAR_1x ${BAR-1}\"", &[("FOO", "x -1}")]);
        assert_parses(
            b"FOO=x # c\n  BAR = \"y\"  # d\n",
            &[("BAR", "y"), ("FOO", "x")],
        );
        assert_parses(b"FOO=  \"x\"", &[("FOO", "x")]);
        assert_parses(b"FOO=x\x0b#c", &[("FOO", "x")]);
        assert_parses(b"FOO=x #", &[("FOO", "x")]);
        assert_parses(b"FOO=1#", &[("FOO", "1#")]);
        assert_parses(b"FOO= #c", &[("FOO", "#c")]);
        assert_parses(b"FOO=\"a\x00b\"", &[("FOO", "a\0b")]);
        // White space: Unicode's before a statement, godotenv's own in one.
        assert_parses("\u{3000}FOO=1".as_bytes(), &[("FOO", "1")]);
        assert_parses("FOO=a\u{3000}#c".as_bytes(), &[("FOO", "a\u{3000}#c")]);
        assert_parses("FOO=a\u{a0}#c".as_bytes(), &[("FOO", "a")]);
        assert_parses("FOO\u{a0}=1".as_bytes(), &[("FOO", "1")]);
        // A name is read as Latin-1: `ê` in UTF-8 is `Ãª`, two letters.
        assert_parses("\u{ea}=1".as_bytes(), &[("\u{ea}", "1")]);
        // An unquoted value is read as runes, a quoted one as bytes.
        assert_parses(b"FOO=a\xffb", &[("FOO", "a\u{fffd}b")]);
        assert_parses(b"FOO=a\xa0#c", &[("FOO", "a\u{fffd}#c")]);
        let quoted = parse(b"FOO=\"a\xffb\"\nBAR=\"\\\xff\"\n\xaa=1").expect("parses");
        let want: Vars = [
            (b"BAR".to_vec(), b"\xff".to_vec()),
            (b"FOO".to_vec(), b"a\xffb".to_vec()),
            (b"\xaa".to_vec(), b"1".to_vec()),
        ]
        .into();
        assert_eq!(quoted, want);

        assert_stops(b"export   ", 1, Problem::ZeroLengthString);
        assert_stops(b"FOO.BAR-BAZ=1", 1, Problem::UnexpectedCharacter(b'-'));
        assert_stops(b"\nexport\nFOO=1", 2, Problem::UnexpectedCharacter(b'\n'));
        assert_stops(b"FOO=\"a\\\\\"", 1, Problem::UnterminatedQuote);
        assert_stops(b"FOO=\"\"\"", 1, Problem::UnexpectedCharacter(b'"'));
        assert_stops(b"FOO=\"a\"\"b\"", 1, Problem::UnexpectedCharacter(b'"'));
        assert_stops(b"FOO=\"unterminated\nBAR=1", 1, Problem::UnterminatedQuote);
        assert_stops(
            b"FOO=\"x\"\n\n# c\n'BAR'=1",
            4,
            Problem::UnexpectedCharacter(b'\''),
        );
        // In a name, as anywhere but at the start of the file.
        assert_stops(
            b"FOO=1\n\xef\xbb\xbfBAR=2",
            2,
            Problem::UnexpectedCharacter(0xbb),
        );
    }

    // Not upstream's: a name's bytes past ASCII are those Go's
    // `unicode.IsLetter` or `unicode.IsNumber` accepts, from a Go probe.
    #[test]
    fn name_bytes_are_gos() {
        let accepted: Vec<u8> = (0..=0xff_u8)
            .filter(|&byte| char::from(byte).is_alphanumeric())
            .collect();
        let mut go: Vec<u8> = (b'0'..=b'9')
            .chain(b'A'..=b'Z')
            .chain(b'a'..=b'z')
            .collect();
        go.extend([0xaa, 0xb2, 0xb3, 0xb5, 0xb9, 0xba, 0xbc, 0xbd, 0xbe]);
        go.extend((0xc0..=0xd6).chain(0xd8..=0xf6).chain(0xf8..=0xff));
        assert_eq!(accepted, go);
    }

    // Not upstream's: godotenv fails on a byte order mark.
    #[test]
    fn a_byte_order_mark_is_skipped() {
        assert_parses(b"\xef\xbb\xbfFOO=1\r\nBAR=2", &[("BAR", "2"), ("FOO", "1")]);
    }

    // Not upstream's: godotenv's errors quote the file.
    #[test]
    fn errors_name_the_line_not_the_text() {
        let cases: [(&[u8], &str); 4] = [
            (
                b"A=1\nB=\"secret-value\nC=2",
                "unterminated quoted value on line 2",
            ),
            (
                b"A=1\r\n\r\nsecret-value=2",
                "unexpected character '-' in variable name on line 3",
            ),
            (
                b"A=\"x\"\"secret\"",
                "unexpected character '\"' in variable name on line 1",
            ),
            (b"# secret\nexport  ", "zero length string on line 2"),
        ];
        for (input, want) in cases {
            let error = parse(input).expect_err("doesn't parse");
            assert_eq!(error.to_string(), want);
            assert!(!format!("{error:?}").contains("secret"), "{error:?}");
        }
    }

    // Not upstream's: what godotenv's loadFile leaves to os.Setenv's errors.
    #[test]
    fn missing_leaves_out_what_set_var_refuses() {
        let vars = parse(b"=\"value\"\nBAR=\"a\x00b\"\nBAZ=1").expect("parses");
        let set = missing(vars, |_| false);
        assert_eq!(set, [(OsString::from("BAZ"), OsString::from("1"))]);
    }

    // Not upstream's: on Unix a value keeps its bytes; on Windows each byte
    // that isn't UTF-8 is U+FFFD, as Go sets it.
    #[test]
    fn values_that_arent_utf8() {
        let set = missing(parse(b"FOO=\"a\xffb\"").expect("parses"), |_| false);
        #[cfg(unix)]
        let want = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(b"a\xffb".to_vec())
        };
        #[cfg(not(unix))]
        let want = OsString::from("a\u{fffd}b");
        assert_eq!(set, [(OsString::from("FOO"), want)]);
    }

    // Not upstream's: on Windows, a name the environment has in another case
    // is set already. godotenv set it, replacing the environment's value.
    #[cfg(windows)]
    #[test]
    fn the_environment_wins_whatever_the_case() {
        let vars = vars(&[("pAtH", "nowhere")]);
        assert_eq!(missing(vars, in_environment), []);
    }
}
