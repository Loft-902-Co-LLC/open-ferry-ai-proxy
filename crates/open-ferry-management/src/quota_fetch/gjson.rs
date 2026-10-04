// Ported from tidwall/gjson v1.18.0 gjson.go (Get, Parse, Result.String,
// Result.Array, Result.Get, Result.Exists, Result.IsArray, arrayOrMap,
// squash, tonum, tolit, tostr, parseString, parseNumber, parseLiteral,
// parseArrayPath, isDotPiperChar, parseObjectPath, parseSquash, parseObject,
// parseArray, parseSubSelectors, execStatic, execModifier, parseUint,
// runeit, unescape) (MIT), as CLIProxyAPI
// internal/api/handlers/management/plugin_quota.go uses it (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/tidwall/gjson

//! The part of gjson that upstream's quota probe reads a response with:
//! `Get` with a path of object keys and array indexes (`#` for an array's
//! length), `Parse`, and a result's `String`, `Array`, `Get` and `Exists`.
//!
//! Like gjson, it expects JSON and doesn't validate it: it scans for what
//! the path names, and on other input finds what gjson finds. A key in a
//! path may escape `.` and the other special characters with a backslash.
//! Where gjson calls itself for each part of a path, the search keeps its
//! place on a stack of its own, so a path as deep as the response (the
//! response may be 16 MiB) is read as gjson reads it, not overflowing the
//! thread's stack.
//!
//! Deviations from upstream:
//! - Where gjson would apply a wildcard, pipe, query, modifier, literal
//!   (`!`), multipath/sub-selector or `..`, this port finds nothing.

use std::borrow::Cow;

use open_ferry_translate::go::{format_float, parse_float, to_lower};

use crate::go::lossy;

/// A result's type (gjson's `Type`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    #[default]
    Null,
    False,
    Number,
    String,
    True,
    Json,
}

/// A value found (gjson's `Result`, less its indexes).
#[derive(Clone, Default)]
pub(crate) struct Value<'a> {
    pub(crate) kind: Kind,
    /// The value's JSON text.
    pub(crate) raw: Cow<'a, [u8]>,
    /// A string's text, unescaped.
    pub(crate) str: Cow<'a, [u8]>,
    /// A number's value.
    pub(crate) num: f64,
}

impl Value<'_> {
    /// Whether the value was found (`Exists`).
    pub(crate) fn exists(&self) -> bool {
        self.kind != Kind::Null || !self.raw.is_empty()
    }

    /// Whether the value is an array (`IsArray`).
    pub(crate) fn is_array(&self) -> bool {
        self.kind == Kind::Json && self.raw.first() == Some(&b'[')
    }

    /// The value as text (`String`): a string's text, a number as written
    /// if it is an integer, else as Go formats it, `true` or `false`, an
    /// object's or array's JSON, or empty.
    pub(crate) fn string(&self) -> Cow<'_, [u8]> {
        match self.kind {
            Kind::Null => Cow::Borrowed(b""),
            Kind::False => Cow::Borrowed(b"false"),
            Kind::True => Cow::Borrowed(b"true"),
            Kind::String => Cow::Borrowed(&self.str),
            Kind::Json => Cow::Borrowed(&self.raw),
            Kind::Number => {
                let digits = self.raw.strip_prefix(b"-").unwrap_or(&self.raw);
                if !self.raw.is_empty() && digits.iter().all(u8::is_ascii_digit) {
                    Cow::Borrowed(&self.raw)
                } else {
                    Cow::Owned(format_float(self.num).into_bytes())
                }
            }
        }
    }

    /// What `path` names within the value (`Result.Get`).
    pub(crate) fn get(&self, path: &str) -> Value<'_> {
        get(&self.raw, path)
    }

    /// An array's elements; a value that isn't an array alone, and nothing
    /// for one not found (`Array`).
    pub(crate) fn array(&self) -> Vec<Value<'_>> {
        if self.kind == Kind::Null {
            return Vec::new();
        }
        if !self.is_array() {
            return vec![self.clone()];
        }
        let json: &[u8] = &self.raw;
        let mut out = Vec::new();
        let mut i = 0;
        while i < json.len() {
            let b = at(json, i);
            i += 1;
            if b == b'[' {
                break;
            }
            if b > b' ' {
                return out;
            }
        }
        while i < json.len() {
            let b = at(json, i);
            if b <= b' ' {
                i += 1;
                continue;
            }
            if b == b']' || b == b'}' {
                break;
            }
            let rest = from(json, i);
            let value = match b {
                b'0'..=b'9' | b'-' => {
                    let (raw, num) = tonum(rest);
                    number(raw, num)
                }
                b'{' | b'[' => json_value(squash(rest)),
                b'n' => literal(Kind::Null, tolit(rest)),
                b't' => literal(Kind::True, tolit(rest)),
                b'f' => literal(Kind::False, tolit(rest)),
                b'"' => {
                    let (raw, str) = tostr(rest);
                    Value {
                        kind: Kind::String,
                        raw: Cow::Borrowed(raw),
                        str,
                        num: 0.0,
                    }
                }
                _ => {
                    i += 1;
                    continue;
                }
            };
            i += value.raw.len().max(1);
            out.push(value);
        }
        out
    }
}

/// The byte at `i`, or 0 past the end.
fn at(json: &[u8], i: usize) -> u8 {
    json.get(i).copied().unwrap_or(0)
}

/// `json[start..]`, or empty.
fn from(json: &[u8], start: usize) -> &[u8] {
    json.get(start..).unwrap_or_default()
}

/// `json[start..end]`, or empty.
fn span(json: &[u8], start: usize, end: usize) -> &[u8] {
    json.get(start..end).unwrap_or_default()
}

fn number(raw: &[u8], num: f64) -> Value<'_> {
    Value {
        kind: Kind::Number,
        raw: Cow::Borrowed(raw),
        str: Cow::Borrowed(b""),
        num,
    }
}

fn json_value(raw: &[u8]) -> Value<'_> {
    literal(Kind::Json, raw)
}

fn literal(kind: Kind, raw: &[u8]) -> Value<'_> {
    Value {
        kind,
        raw: Cow::Borrowed(raw),
        str: Cow::Borrowed(b""),
        num: 0.0,
    }
}

/// A string value from its JSON text, quotes included.
fn string_value(raw: &[u8], escaped: bool) -> Value<'_> {
    let inner = span(raw, 1, raw.len().saturating_sub(1));
    Value {
        kind: Kind::String,
        raw: Cow::Borrowed(raw),
        str: if escaped {
            Cow::Owned(unescape(inner))
        } else {
            Cow::Borrowed(inner)
        },
        num: 0.0,
    }
}

/// A number as gjson reads one: Go's `strconv.ParseFloat`, its error
/// ignored.
fn to_number(raw: &[u8]) -> f64 {
    std::str::from_utf8(raw).map_or(0.0, parse_float)
}

/// The value `json` starts with (`Parse`).
pub(crate) fn parse(json: &[u8]) -> Value<'_> {
    let mut i = 0;
    while i < json.len() {
        let b = at(json, i);
        if b == b'{' || b == b'[' {
            return json_value(from(json, i));
        }
        if b <= b' ' {
            i += 1;
            continue;
        }
        let rest = from(json, i);
        return match b {
            b'+' | b'-' | b'0'..=b'9' | b'i' | b'I' | b'N' => {
                let (raw, num) = tonum(rest);
                number(raw, num)
            }
            b'n' if i + 1 < json.len() && at(json, i + 1) != b'u' => {
                let (raw, num) = tonum(rest);
                number(raw, num)
            }
            b'n' => literal(Kind::Null, tolit(rest)),
            b't' => literal(Kind::True, tolit(rest)),
            b'f' => literal(Kind::False, tolit(rest)),
            b'"' => {
                let (raw, str) = tostr(rest);
                Value {
                    kind: Kind::String,
                    raw: Cow::Borrowed(raw),
                    str,
                    num: 0.0,
                }
            }
            _ => Value::default(),
        };
    }
    Value::default()
}

/// What `path` names in `json` (`Get`).
pub(crate) fn get<'a>(json: &'a [u8], path: &str) -> Value<'a> {
    let mut path = path.as_bytes();
    if path.len() > 1 {
        match path.first() {
            Some(b'@') if is_modifier(name_until(from(path, 1), b":|.")) => {
                return Value::default();
            }
            Some(b'!') if is_static(path) => return Value::default(),
            Some(b'[' | b'{') => match sub_selectors(path) {
                Some(rest) if rest.is_empty() || matches!(rest.first(), Some(b'|' | b'.')) => {
                    return Value::default();
                }
                Some(rest) => path = rest,
                None => path = b"",
            },
            _ => {}
        }
    }
    if path.starts_with(b"..") {
        return Value::default();
    }
    let mut c = Context {
        json,
        value: Value::default(),
        unsupported: false,
    };
    for (i, &b) in json.iter().enumerate() {
        if b == b'{' || b == b'[' {
            search(&mut c, b == b'{', i + 1, path);
            break;
        }
    }
    if c.unsupported {
        Value::default()
    } else {
        c.value
    }
}

/// A search in progress (gjson's `parseContext`).
struct Context<'a> {
    json: &'a [u8],
    value: Value<'a>,
    /// The path asks for what this port leaves out.
    unsupported: bool,
}

/// gjson's modifiers.
fn is_modifier(name: &[u8]) -> bool {
    matches!(
        name,
        b"pretty"
            | b"ugly"
            | b"reverse"
            | b"this"
            | b"flatten"
            | b"join"
            | b"valid"
            | b"keys"
            | b"values"
            | b"tostr"
            | b"fromstr"
            | b"group"
            | b"dig"
    )
}

/// `s` up to the first of `stops`.
fn name_until<'p>(s: &'p [u8], stops: &[u8]) -> &'p [u8] {
    let end = s.iter().position(|b| stops.contains(b)).unwrap_or(s.len());
    span(s, 0, end)
}

/// Whether a path starting `!` names a literal value (`execStatic`).
fn is_static(path: &[u8]) -> bool {
    let name = from(path, 1);
    if matches!(
        name.first(),
        Some(b'{' | b'[' | b'"' | b'+' | b'-' | b'0'..=b'9')
    ) {
        return true;
    }
    let name = name_until(name, b"|.");
    matches!(
        to_lower(&lossy(name)).as_str(),
        "true" | "false" | "null" | "nan" | "inf"
    )
}

/// The rest of a path after its sub-selectors, if they close
/// (`parseSubSelectors`).
fn sub_selectors(path: &[u8]) -> Option<&[u8]> {
    let mut depth = 1;
    let mut i = 1;
    while i < path.len() {
        match at(path, i) {
            b'\\' => i += 1,
            b'"' => {
                i += 1;
                while i < path.len() {
                    match at(path, i) {
                        b'\\' => i += 1,
                        b'"' => break,
                        _ => {}
                    }
                    i += 1;
                }
            }
            b'[' | b'(' | b'{' => depth += 1,
            b']' | b')' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(from(path, i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Whether the part after a dot starts a modifier or sub-selector
/// (`isDotPiperChar`).
fn is_dot_piper(s: &[u8]) -> bool {
    match s.first() {
        Some(b'@') => is_modifier(name_until(from(s, 1), b".|:")),
        Some(b'[' | b'{') => true,
        _ => false,
    }
}

/// The first part of a path within an object (`objectPathResult`).
struct ObjectPath<'p> {
    part: Cow<'p, [u8]>,
    path: &'p [u8],
    piped: bool,
    wild: bool,
    more: bool,
}

/// `parseObjectPath`.
fn parse_object_path(path: &[u8]) -> ObjectPath<'_> {
    let mut r = ObjectPath {
        part: Cow::Borrowed(path),
        path: b"",
        piped: false,
        wild: false,
        more: false,
    };
    let mut i = 0;
    while i < path.len() {
        match at(path, i) {
            b'|' => {
                r.part = Cow::Borrowed(span(path, 0, i));
                r.piped = true;
                return r;
            }
            b'.' => {
                r.part = Cow::Borrowed(span(path, 0, i));
                split_dot(&mut r, path, i);
                return r;
            }
            b'*' | b'?' => r.wild = true,
            b'\\' => {
                let mut part = span(path, 0, i).to_vec();
                i += 1;
                if i < path.len() {
                    part.push(at(path, i));
                    i += 1;
                    while i < path.len() {
                        match at(path, i) {
                            b'\\' => {
                                i += 1;
                                if i < path.len() {
                                    part.push(at(path, i));
                                }
                                i += 1;
                                continue;
                            }
                            b'.' => {
                                r.part = Cow::Owned(part);
                                split_dot(&mut r, path, i);
                                return r;
                            }
                            b'|' => {
                                r.part = Cow::Owned(part);
                                r.piped = true;
                                return r;
                            }
                            b'*' | b'?' => r.wild = true,
                            _ => {}
                        }
                        part.push(at(path, i));
                        i += 1;
                    }
                }
                r.part = Cow::Owned(part);
                return r;
            }
            _ => {}
        }
        i += 1;
    }
    r
}

/// What follows the dot at `i`: a pipe, or the rest of the path.
fn split_dot<'p>(r: &mut ObjectPath<'p>, path: &'p [u8], i: usize) {
    if i + 1 < path.len() && is_dot_piper(from(path, i + 1)) {
        r.piped = true;
    } else {
        r.path = from(path, i + 1);
        r.more = true;
    }
}

/// The first part of a path within an array (`arrayPathResult`).
struct ArrayPath<'p> {
    part: &'p [u8],
    path: &'p [u8],
    piped: bool,
    more: bool,
    arrch: bool,
    /// A query or `#.` path, which this port leaves out.
    unsupported: bool,
}

/// `parseArrayPath`.
fn parse_array_path(path: &[u8]) -> ArrayPath<'_> {
    let mut r = ArrayPath {
        part: path,
        path: b"",
        piped: false,
        more: false,
        arrch: false,
        unsupported: false,
    };
    for (i, &b) in path.iter().enumerate() {
        match b {
            b'|' => {
                r.part = span(path, 0, i);
                r.piped = true;
                return r;
            }
            b'.' => {
                r.part = span(path, 0, i);
                if !r.arrch && i + 1 < path.len() && is_dot_piper(from(path, i + 1)) {
                    r.piped = true;
                } else {
                    r.path = from(path, i + 1);
                    r.more = true;
                }
                return r;
            }
            b'#' => {
                r.arrch = true;
                if i == 0 && path.len() > 1 && matches!(at(path, 1), b'.' | b'[' | b'(') {
                    r.unsupported = true;
                    return r;
                }
            }
            _ => {}
        }
    }
    r
}

/// Go's `parseUint` in gjson: decimal digits, wrapping past 64 bits.
fn parse_uint(s: &[u8]) -> Option<u64> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(s.iter().fold(0u64, |n, &b| {
        n.wrapping_mul(10).wrapping_add(u64::from(b - b'0'))
    }))
}

/// What one step of a container's scan came to.
enum Step<'p> {
    /// The scan ended at `next`, with the value found or not.
    Done { next: usize, found: bool },
    /// The scan met an object or array along the path, whose members start
    /// at `next`: that is searched for `path` and then the scan goes on.
    Descend {
        next: usize,
        object: bool,
        path: &'p [u8],
    },
}

fn done<'p>(next: usize, found: bool) -> Step<'p> {
    Step::Done { next, found }
}

/// A container being searched: one level of what gjson does by recursion,
/// kept on a stack of its own, so that a path as deep as the body can't
/// overflow the thread's.
enum Scan<'a, 'p> {
    Object(ObjectScan<'a, 'p>),
    Array(ArrayScan<'p>),
}

impl<'a, 'p> Scan<'a, 'p> {
    /// The scan of the object or array whose members follow, or `None`
    /// where the path asks for what this port leaves out (`c.unsupported` is
    /// set then).
    fn open(c: &mut Context<'a>, object: bool, path: &'p [u8]) -> Option<Self> {
        if object {
            let rp = parse_object_path(path);
            if rp.wild || rp.piped {
                c.unsupported = true;
                return None;
            }
            Some(Self::Object(ObjectScan {
                rp,
                ok: false,
                key: b"",
                key_escaped: false,
            }))
        } else {
            let rp = parse_array_path(path);
            if rp.unsupported || rp.piped {
                c.unsupported = true;
                return None;
            }
            // Go converts the index to an int, wrapping.
            let partidx = if rp.arrch {
                0
            } else {
                parse_uint(rp.part).map_or(-1, |n| n as i64)
            };
            Some(Self::Array(ArrayScan {
                rp,
                partidx,
                h: 0,
                pmatch: false,
                hit: false,
            }))
        }
    }

    /// Scans on from `i`: where the container's members start, or where the
    /// search of one it descended into ended.
    fn run(&mut self, c: &mut Context<'a>, i: usize) -> Step<'p> {
        match self {
            Self::Object(scan) => scan.run(c, i),
            Self::Array(scan) => scan.run(c, i),
        }
    }
}

/// Searches the object or array whose members start at `start` for `path`
/// (`parseObject` and `parseArray`, which in gjson call each other).
fn search<'a, 'p>(c: &mut Context<'a>, object: bool, start: usize, path: &'p [u8]) {
    let mut stack: Vec<Scan<'a, 'p>> = Vec::new();
    let mut opening = Some((object, path));
    let mut next = start;
    loop {
        if let Some((object, path)) = opening.take()
            && let Some(scan) = Scan::open(c, object, path)
        {
            stack.push(scan);
        }
        // Where a container wasn't opened, its parent goes on from `next`,
        // as it does where gjson's call returns.
        let Some(scan) = stack.last_mut() else {
            return;
        };
        match scan.run(c, next) {
            Step::Done { found: true, .. } => return,
            Step::Done { next: end, .. } => {
                stack.pop();
                next = end;
            }
            Step::Descend {
                next: inner,
                object,
                path,
            } => {
                opening = Some((object, path));
                next = inner;
            }
        }
    }
}

/// An object being searched (`parseObject`).
struct ObjectScan<'a, 'p> {
    rp: ObjectPath<'p>,
    ok: bool,
    key: &'a [u8],
    key_escaped: bool,
}

impl<'a, 'p> ObjectScan<'a, 'p> {
    fn run(&mut self, c: &mut Context<'a>, mut i: usize) -> Step<'p> {
        let json = c.json;
        while i < json.len() {
            while i < json.len() {
                match at(json, i) {
                    b'"' => {
                        let (next, raw, escaped, found) = parse_string(json, i + 1);
                        i = next;
                        self.ok = found;
                        self.key_escaped = escaped;
                        self.key = span(raw, 1, raw.len().saturating_sub(1));
                        break;
                    }
                    b'}' => return done(i + 1, false),
                    _ => i += 1,
                }
            }
            if !self.ok {
                return done(i, false);
            }
            let pmatch = if self.key_escaped {
                unescape(self.key) == *self.rp.part
            } else {
                self.key == &*self.rp.part
            };
            let hit = pmatch && !self.rp.more;
            while i < json.len() {
                let ch = at(json, i);
                let mut num = false;
                match ch {
                    b'"' => {
                        let (next, raw, escaped, found) = parse_string(json, i + 1);
                        i = next;
                        self.ok = found;
                        if !self.ok {
                            return done(i, false);
                        }
                        if hit {
                            c.value = string_value(raw, escaped);
                            return done(i, true);
                        }
                    }
                    b'{' | b'[' => {
                        if pmatch && !hit {
                            return Step::Descend {
                                next: i + 1,
                                object: ch == b'{',
                                path: self.rp.path,
                            };
                        }
                        let (next, raw) = parse_squash(json, i);
                        i = next;
                        if hit {
                            c.value = json_value(raw);
                            return done(i, true);
                        }
                    }
                    b'n' if i + 1 < json.len() && at(json, i + 1) != b'u' => num = true,
                    b'n' | b't' | b'f' => {
                        let (next, raw) = parse_literal(json, i);
                        i = next;
                        if hit {
                            c.value = literal(literal_kind(ch), raw);
                            return done(i, true);
                        }
                    }
                    b'+' | b'-' | b'0'..=b'9' | b'i' | b'I' | b'N' => num = true,
                    _ => {
                        i += 1;
                        continue;
                    }
                }
                if num {
                    let (next, raw) = parse_number(json, i);
                    i = next;
                    if hit {
                        c.value = number(raw, to_number(raw));
                        return done(i, true);
                    }
                }
                break;
            }
        }
        done(i, false)
    }
}

/// The kind of the literal starting with `first`.
fn literal_kind(first: u8) -> Kind {
    match first {
        b't' => Kind::True,
        b'f' => Kind::False,
        _ => Kind::Null,
    }
}

/// An array being searched (`parseArray`).
struct ArrayScan<'p> {
    rp: ArrayPath<'p>,
    partidx: i64,
    /// The index of the element after the one being read.
    h: i64,
    pmatch: bool,
    hit: bool,
}

impl<'p> ArrayScan<'p> {
    fn run(&mut self, c: &mut Context<'_>, mut i: usize) -> Step<'p> {
        let json = c.json;
        let len = json.len();
        while i < len + 1 {
            if !self.rp.arrch {
                self.pmatch = self.partidx == self.h;
                self.hit = self.pmatch && !self.rp.more;
            }
            self.h += 1;
            loop {
                let ch = match i.cmp(&len) {
                    std::cmp::Ordering::Greater => break,
                    std::cmp::Ordering::Equal => b']',
                    std::cmp::Ordering::Less => at(json, i),
                };
                let mut num = false;
                match ch {
                    b'"' => {
                        let (next, raw, escaped, found) = parse_string(json, i + 1);
                        i = next;
                        if !found {
                            return done(i, false);
                        }
                        if self.hit {
                            c.value = string_value(raw, escaped);
                            return done(i, true);
                        }
                    }
                    b'{' | b'[' => {
                        if self.pmatch && !self.hit {
                            return Step::Descend {
                                next: i + 1,
                                object: ch == b'{',
                                path: self.rp.path,
                            };
                        }
                        let (next, raw) = parse_squash(json, i);
                        i = next;
                        if self.hit {
                            c.value = json_value(raw);
                            return done(i, true);
                        }
                    }
                    b'n' if i + 1 < len && at(json, i + 1) != b'u' => num = true,
                    b'n' | b't' | b'f' => {
                        let (next, raw) = parse_literal(json, i);
                        i = next;
                        if self.hit {
                            c.value = literal(literal_kind(ch), raw);
                            return done(i, true);
                        }
                    }
                    b'+' | b'-' | b'0'..=b'9' | b'i' | b'I' | b'N' => num = true,
                    b']' => {
                        if self.rp.arrch && self.rp.part == b"#" {
                            c.value = Value {
                                kind: Kind::Number,
                                raw: Cow::Owned((self.h - 1).to_string().into_bytes()),
                                str: Cow::Borrowed(b""),
                                num: (self.h - 1) as f64,
                            };
                            return done(i + 1, true);
                        }
                        return done(i + 1, false);
                    }
                    _ => {
                        i += 1;
                        continue;
                    }
                }
                if num {
                    let (next, raw) = parse_number(json, i);
                    i = next;
                    if self.hit {
                        c.value = number(raw, to_number(raw));
                        return done(i, true);
                    }
                }
                break;
            }
        }
        done(i, false)
    }
}

/// Whether the quote at `i` is escaped: preceded by an odd number of
/// backslashes, counting none before `floor`.
fn escaped(json: &[u8], floor: usize, i: usize) -> bool {
    let mut n = 0;
    let mut k = i;
    while k > floor && at(json, k - 1) == b'\\' {
        n += 1;
        k -= 1;
    }
    n % 2 == 1
}

/// The string whose text starts at `i`, just after its opening quote:
/// where it ends, its JSON text, whether it holds an escape, and whether
/// it closes (`parseString`).
fn parse_string(json: &[u8], mut i: usize) -> (usize, &[u8], bool, bool) {
    let start = i.saturating_sub(1);
    while i < json.len() {
        match at(json, i) {
            b'"' => return (i + 1, span(json, start, i + 1), false, true),
            b'\\' => {
                i += 1;
                while i < json.len() {
                    if at(json, i) == b'"' && !escaped(json, 1, i) {
                        return (i + 1, span(json, start, i + 1), true, true);
                    }
                    i += 1;
                }
                break;
            }
            _ => i += 1,
        }
    }
    (i, from(json, start), false, false)
}

/// The number starting at `i` (`parseNumber`).
fn parse_number(json: &[u8], i: usize) -> (usize, &[u8]) {
    let end = (i + 1..json.len())
        .find(|&j| matches!(at(json, j), 0..=b' ' | b',' | b']' | b'}'))
        .unwrap_or(json.len());
    (end, span(json, i, end))
}

/// The literal starting at `i` (`parseLiteral`).
fn parse_literal(json: &[u8], i: usize) -> (usize, &[u8]) {
    let end = (i + 1..json.len())
        .find(|&j| !at(json, j).is_ascii_lowercase())
        .unwrap_or(json.len());
    (end, span(json, i, end))
}

/// The object or array opening at `i`, to its close (`parseSquash`).
fn parse_squash(json: &[u8], mut i: usize) -> (usize, &[u8]) {
    let start = i;
    i += 1;
    let mut depth = 1;
    while i < json.len() {
        match at(json, i) {
            b'"' => {
                i += 1;
                let first = i;
                while i < json.len() && !(at(json, i) == b'"' && !escaped(json, first, i)) {
                    i += 1;
                }
            }
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => {
                depth -= 1;
                if depth == 0 {
                    i += 1;
                    return (i, span(json, start, i));
                }
            }
            _ => {}
        }
        i += 1;
    }
    (i, from(json, start))
}

/// The number `json` starts with, and its value (`tonum`).
fn tonum(json: &[u8]) -> (&[u8], f64) {
    let end = (1..json.len())
        .find(|&i| matches!(at(json, i), 0..=b' ' | b',' | b']' | b'}'))
        .unwrap_or(json.len());
    let raw = span(json, 0, end);
    (raw, to_number(raw))
}

/// The literal `json` starts with (`tolit`).
fn tolit(json: &[u8]) -> &[u8] {
    let end = (1..json.len())
        .find(|&i| !at(json, i).is_ascii_lowercase())
        .unwrap_or(json.len());
    span(json, 0, end)
}

/// The string `json` starts with: its JSON text and its text (`tostr`).
fn tostr(json: &[u8]) -> (&[u8], Cow<'_, [u8]>) {
    let mut i = 1;
    while i < json.len() {
        match at(json, i) {
            b'"' => return (span(json, 0, i + 1), Cow::Borrowed(span(json, 1, i))),
            b'\\' => {
                i += 1;
                while i < json.len() {
                    if at(json, i) == b'"' && !escaped(json, 1, i) {
                        return (span(json, 0, i + 1), Cow::Owned(unescape(span(json, 1, i))));
                    }
                    i += 1;
                }
                return (json, Cow::Owned(unescape(span(json, 1, i))));
            }
            _ => i += 1,
        }
    }
    (json, Cow::Borrowed(from(json, 1)))
}

/// The value `json` starts with, `[`, `{`, `(` or `"`, to its close
/// (`squash`).
fn squash(json: &[u8]) -> &[u8] {
    let (mut i, mut depth) = if at(json, 0) == b'"' { (0, 0) } else { (1, 1) };
    while i < json.len() {
        match at(json, i) {
            b'"' => {
                i += 1;
                let first = i;
                while i < json.len() && !(at(json, i) == b'"' && !escaped(json, first, i)) {
                    i += 1;
                }
                if depth == 0 {
                    if i >= json.len() {
                        return json;
                    }
                    return span(json, 0, i + 1);
                }
            }
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => {
                depth -= 1;
                if depth == 0 {
                    return span(json, 0, i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    json
}

/// Four hex digits as a code point, or 0 (`runeit`).
fn runeit(json: &[u8]) -> u32 {
    std::str::from_utf8(span(json, 0, 4))
        .ok()
        .filter(|hex| hex.len() == 4 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .unwrap_or(0)
}

/// A string's text with its escapes undone, stopping at a control
/// character or a bad escape (`unescape`).
fn unescape(json: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(json.len());
    let mut i = 0;
    while i < json.len() {
        let b = at(json, i);
        if b < b' ' {
            return out;
        }
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&escape) = json.get(i) else {
            return out;
        };
        match escape {
            b'\\' | b'/' | b'"' => out.push(escape),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                if i + 5 > json.len() {
                    return out;
                }
                let mut r = runeit(from(json, i + 1));
                i += 5;
                if (0xd800..0xe000).contains(&r)
                    && json.len().saturating_sub(i) >= 6
                    && at(json, i) == b'\\'
                    && at(json, i + 1) == b'u'
                {
                    // gjson consumes the next escape, as utf16.DecodeRune
                    // reads it, whatever it is.
                    let low = runeit(from(json, i + 2));
                    r = if (0xd800..0xdc00).contains(&r) && (0xdc00..0xe000).contains(&low) {
                        0x10000 + ((r - 0xd800) << 10) + (low - 0xdc00)
                    } else {
                        0xfffd
                    };
                    i += 6;
                }
                let c = char::from_u32(r).unwrap_or(char::REPLACEMENT_CHARACTER);
                let mut buf = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                // Back up for the loop's step.
                i -= 1;
            }
            _ => return out,
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What gjson's `Get` gives, as gjson v1.18.0 itself gave it for
    /// these paths: whether found, the type, `String` and `Raw`.
    fn found(json: &str, path: &str) -> (bool, Kind, String, String) {
        let value = get(json.as_bytes(), path);
        (
            value.exists(),
            value.kind,
            String::from_utf8_lossy(&value.string()).into_owned(),
            String::from_utf8_lossy(&value.raw).into_owned(),
        )
    }

    fn text(json: &str, path: &str) -> Option<String> {
        let (exists, _, string, _) = found(json, path);
        exists.then_some(string)
    }

    // Not upstream's: gjson's paths as the quota probe uses them.
    #[test]
    fn paths_find_keys_and_indexes() {
        let json = r#"{"a":{"b":"x","n":1.50,"i":-7,"t":true,"f":false,"z":null},"l":[5,6,7],"o":[{"k":"v"}]}"#;
        assert_eq!(text(json, "a.b").as_deref(), Some("x"));
        assert_eq!(text(json, "a.n").as_deref(), Some("1.5"));
        assert_eq!(found(json, "a.n").3, "1.50");
        assert_eq!(text(json, "a.i").as_deref(), Some("-7"));
        assert_eq!(found(json, "a.t").1, Kind::True);
        assert_eq!(found(json, "a.f").1, Kind::False);
        assert_eq!(
            found(json, "a.z"),
            (true, Kind::Null, String::new(), "null".into())
        );
        assert_eq!(text(json, "a.missing"), None);
        assert_eq!(text(json, "l.1").as_deref(), Some("6"));
        assert_eq!(text(json, "l.01").as_deref(), Some("6"));
        assert_eq!(text(json, "l.18446744073709551617").as_deref(), Some("6"));
        assert_eq!(text(json, "l.3"), None);
        assert_eq!(text(json, "l.#").as_deref(), Some("3"));
        assert_eq!(text(json, "o.0.k").as_deref(), Some("v"));
        assert_eq!(
            text(json, "a").as_deref(),
            Some(r#"{"b":"x","n":1.50,"i":-7,"t":true,"f":false,"z":null}"#)
        );
        assert_eq!(text(json, "l").as_deref(), Some("[5,6,7]"));
        // The first of two equal keys holding the rest of the path wins
        // only if the path is found in it.
        assert_eq!(
            text(r#"{"a":{"c":1},"a":{"b":"x"}}"#, "a.b").as_deref(),
            Some("x")
        );
        assert_eq!(text(r#"{"a":1,"a":2}"#, "a").as_deref(), Some("1"));
        assert_eq!(text(r#"{"a.b":1}"#, r"a\.b").as_deref(), Some("1"));
        assert_eq!(text(r#"{"a":1}"#, "a\\").as_deref(), Some("1"));
        assert_eq!(text(r#"{"":5}"#, "").as_deref(), Some("5"));
        assert_eq!(text("[5]", "0").as_deref(), Some("5"));
        assert_eq!(text("5", "a"), None);
    }

    // Not upstream's: a path as deep as the body is searched without
    // recursing, as gjson's recursion runs on Go's growing stack. The thread
    // here has a stack that couldn't hold a call for each of the levels; the
    // answers are gjson v1.18.0's own for these inputs.
    #[test]
    fn deep_paths_are_searched_without_recursing() {
        const DEPTH: usize = 3_000;
        let joined = |part: &str, count: usize| vec![part; count].join(".");
        let probe = move || {
            // `{"a":{"a":...{"a":"Pro"}...}}`
            let nested = format!("{}\"Pro\"{}", "{\"a\":".repeat(DEPTH), "}".repeat(DEPTH));
            assert_eq!(
                found(&nested, &joined("a", DEPTH)),
                (true, Kind::String, "Pro".into(), "\"Pro\"".into())
            );
            let (exists, kind, _, raw) = found(&nested, &joined("a", DEPTH - 1));
            assert_eq!((exists, kind), (true, Kind::Json));
            assert_eq!(raw, "{\"a\":\"Pro\"}");
            assert_eq!(
                text(&nested, &format!("{}.b", joined("a", DEPTH - 1))),
                None
            );
            assert_eq!(text(&nested, &format!("{}.a", joined("a", DEPTH))), None);
            assert_eq!(
                text(&nested, &format!("{}.#", joined("a", DEPTH - 1))),
                None
            );

            // `[{"a":[{"a":...[{"a":"Pro"}]...}]}]`, reached by `0.a.0.a...`
            let nested = format!(
                "{}\"Pro\"{}",
                "[{\"a\":".repeat(DEPTH / 2),
                "}]".repeat(DEPTH / 2)
            );
            let path = vec!["0.a"; DEPTH / 2].join(".");
            assert_eq!(text(&nested, &path).as_deref(), Some("Pro"));
            assert_eq!(
                text(&nested, path.strip_suffix(".a").unwrap_or_default()).as_deref(),
                Some("{\"a\":\"Pro\"}")
            );
            let shorter = vec!["0.a"; DEPTH / 2 - 1].join(".");
            assert_eq!(text(&nested, &format!("{shorter}.1.a")), None);
            assert_eq!(text(&nested, &format!("{shorter}.0.#")), None);

            // Each level holds an earlier `a` that doesn't hold the rest of
            // the path, so every level is resumed after a failed descent.
            let nested = format!(
                "{}\"Pro\"{}",
                "{\"a\":{\"x\":1},\"a\":".repeat(1_000),
                "}".repeat(1_000)
            );
            assert_eq!(
                text(&nested, &joined("a", 1_000)).as_deref(),
                Some("{\"x\":1}")
            );
            assert_eq!(
                found(&nested, &format!("{}.x", joined("a", 1_000))),
                (true, Kind::Number, "1".into(), "1".into())
            );
        };
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(probe)
            .expect("spawn the probe thread")
            .join()
            .expect("the probe finished");
    }

    // Not upstream's: the paths this port leaves out find nothing.
    #[test]
    fn unsupported_paths_find_nothing() {
        let json = r#"{"a":[{"b":1},{"b":2}],"c":"x","@this":3,"!true":4}"#;
        for path in [
            "a.#.b",
            "a.#(b==1)",
            "c|@reverse",
            "@this",
            "a.@reverse",
            "!true",
            "!1",
            "[c]",
            "{c}",
            "..0",
            "c*",
            "c?",
            "a|0",
        ] {
            assert_eq!(text(json, path), None, "{path}");
        }
        // A name that isn't a modifier or literal is a key.
        assert_eq!(text(r#"{"@x":1}"#, "@x").as_deref(), Some("1"));
        assert_eq!(text(r#"{"!x":1}"#, "!x").as_deref(), Some("1"));
        // Sub-selectors that don't close leave an empty path, which names
        // the empty key.
        assert_eq!(text(r#"{"":7}"#, "[c").as_deref(), Some("7"));
        // Sub-selectors followed by a key name that key.
        assert_eq!(text(r#"{"c":8}"#, "[x]c").as_deref(), Some("8"));
    }

    // Not upstream's: strings are unescaped as gjson unescapes them.
    #[test]
    fn strings_are_unescaped_as_gjson_does() {
        let bs = '\u{5c}';
        let json = format!(
            r#"{{"s":"x{bs}ud800{bs}u0041y","t":"a{bs}n{bs}u00e9","u":"{bs}ud83d{bs}ude00"}}"#
        );
        assert_eq!(text(&json, "s").as_deref(), Some("x\u{fffd}y"));
        assert_eq!(text(&json, "t").as_deref(), Some("a\n\u{e9}"));
        assert_eq!(text(&json, "u").as_deref(), Some("\u{1f600}"));
        // Raw bytes pass through.
        let value = get(b"{\"s\":\"\xff\"}", "s");
        assert_eq!(&*value.string(), b"\xff");
        // Escaped quotes end neither keys nor values.
        let json = format!(r#"{{"k{bs}"":"v{bs}"w","x":1}}"#);
        assert_eq!(text(&json, "k\"").as_deref(), Some("v\"w"));
        assert_eq!(text(&json, "x").as_deref(), Some("1"));
    }

    // Not upstream's: `Array` and `Parse`.
    #[test]
    fn arrays_and_parse() {
        let value = parse(br#" [1, "a", {"b" : [2]}, [3], true, false, null, -2.5e1] "#);
        assert!(value.is_array());
        let items: Vec<_> = value
            .array()
            .iter()
            .map(|item| (item.kind, String::from_utf8_lossy(&item.raw).into_owned()))
            .collect();
        assert_eq!(
            items,
            [
                (Kind::Number, "1".to_owned()),
                (Kind::String, "\"a\"".to_owned()),
                (Kind::Json, "{\"b\" : [2]}".to_owned()),
                (Kind::Json, "[3]".to_owned()),
                (Kind::True, "true".to_owned()),
                (Kind::False, "false".to_owned()),
                (Kind::Null, "null".to_owned()),
                (Kind::Number, "-2.5e1".to_owned()),
            ]
        );
        assert_eq!(value.array()[7].num, -25.0);
        assert_eq!(&*value.array()[7].string(), b"-25");
        assert!(parse(b"").array().is_empty());
        assert_eq!(parse(b"\"s\"").array().len(), 1);
        assert_eq!(parse(b"nan").kind, Kind::Number);
        assert_eq!(parse(b"null").kind, Kind::Null);
        assert!(parse(b"null").exists());
        assert!(!parse(b"x").exists());
        assert_eq!(parse(b"1e400").num, f64::INFINITY);
        assert_eq!(&*parse(b"1e400").string(), b"+Inf");
    }
}
