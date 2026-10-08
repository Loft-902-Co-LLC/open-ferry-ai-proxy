// Ported from tidwall/gjson v1.18.0 gjson.go (Get, parseObjectPath,
// parseArrayPath, parseQuery, isDotPiperChar, parseObject, parseArray,
// queryMatches, trueish, falseish, nullish, parseUint, unescape, trim) and
// tidwall/match v1.1.1 match.go (MatchLimit, match, matchTrimSuffix) (MIT),
// as CLIProxyAPI uses them in internal/runtime/executor/helps/
// payload_helpers.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/tidwall/gjson
// https://github.com/tidwall/match

//! gjson's `Get` over a parsed body, as far as the payload rules use it:
//! keys with `*` and `?` wildcards and `\` escapes, array indexes, `#`
//! counts, `#.key` projections and `#(query)` / `#(query)#` searches with
//! their `==`, `!=`, `<`, `<=`, `>`, `>=`, `%`, `!%` and `~` comparisons.
//!
//! A value found is [`Found::At`] with where it is in the body, so that a
//! set can replace it; a count is [`Found::Count`] and a projection or
//! `#(...)#` search [`Found::List`].
//!
//! Deviations from upstream:
//! - Modifiers (`@reverse` and the like), literals (`!true`), multipaths
//!   (`[a,b]`, `{a,b}`), JSON lines (`..`), pipes (`|`), a `.` before a
//!   modifier, `[` or `{`, an unbalanced query, and a path into a string
//!   holding `{` or `[` (where gjson reads on into the string's text) find
//!   nothing.
//! - A projection's or search's place in the body is known exactly, where
//!   upstream works it out from byte offsets. Upstream's offsets are
//!   relative for a list reached through a `#(query)` with a path after it
//!   (`#(a==1).b.#.c`), so such a list is marked as not placed
//!   ([`Found::List`]), and a set leaves it alone (see `sjson`).

use std::borrow::Cow;

use open_ferry_translate::go::{parse_float, to_lower};
use serde_json::{Map, Number, Value};

/// One step from a value to one inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Key(String),
    Index(usize),
}

/// Where a value is in the body, from its root.
pub(super) type Loc = Vec<Step>;

/// What a path finds (a gjson `Result` that exists).
#[derive(Clone, Debug)]
pub(super) enum Found<'a> {
    /// A value of the body, and where it is.
    At(&'a Value, Loc),
    /// The length of an array (`#`).
    Count(usize),
    /// What a projection (`#.key`) or a `#(query)#` search found, and
    /// whether each item's place in the body is the one upstream writes
    /// to.
    List(Vec<Found<'a>>, bool),
}

impl Found<'_> {
    /// The value found, as gjson's `Raw` reads: a count is a number, a list
    /// an array of its items.
    pub(super) fn to_value(&self) -> Value {
        match self {
            Self::At(value, _) => (*value).clone(),
            Self::Count(count) => Value::Number(Number::from(*count)),
            Self::List(items, _) => Value::Array(items.iter().map(Found::to_value).collect()),
        }
    }

    /// The items of an array found (gjson's `IsArray` then `Array`), or
    /// `None` if it isn't one.
    pub(super) fn array_items(&self) -> Option<Vec<Cow<'_, Value>>> {
        match self {
            Self::At(Value::Array(items), _) => Some(items.iter().map(Cow::Borrowed).collect()),
            Self::List(items, _) => Some(
                items
                    .iter()
                    .map(|item| Cow::Owned(item.to_value()))
                    .collect(),
            ),
            _ => None,
        }
    }
}

/// A path gjson reads in a way this port doesn't.
#[derive(Debug)]
pub(super) struct Unsupported;

/// What `path` finds in `doc` (gjson's `Get`), or `None` when it finds
/// nothing or uses syntax this port doesn't read.
pub(super) fn get<'a>(doc: &'a Value, path: &str) -> Option<Found<'a>> {
    get_from(doc, path, &Loc::new()).ok().flatten()
}

/// What `path` finds in an array holding `items` (gjson's `Get` of
/// `[item, ...]`).
pub(super) fn get_in_items<'a>(items: &'a [Value], path: &str) -> Option<Found<'a>> {
    if unsupported_start(path) {
        return None;
    }
    let mut ctx = Ctx::default();
    parse_array(&mut ctx, items, path, &Loc::new());
    if ctx.unsupported { None } else { ctx.value }
}

/// What `path` finds in `doc`, which is at `base` in the body.
fn get_from<'a>(doc: &'a Value, path: &str, base: &Loc) -> Result<Option<Found<'a>>, Unsupported> {
    if unsupported_start(path) {
        return Err(Unsupported);
    }
    let mut ctx = Ctx::default();
    match doc {
        Value::Object(map) => {
            parse_object(&mut ctx, map, path, base);
        }
        Value::Array(items) => {
            parse_array(&mut ctx, items, path, base);
        }
        // gjson reads on into a string's text looking for `{` or `[`.
        Value::String(text) if text.contains(['{', '[']) => return Err(Unsupported),
        _ => return Ok(None),
    }
    if ctx.unsupported {
        Err(Unsupported)
    } else {
        Ok(ctx.value)
    }
}

/// Whether `path` starts with syntax `Get` reads before any key: a
/// modifier, a literal, a multipath or JSON lines.
fn unsupported_start(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() > 1 && matches!(bytes.first(), Some(b'@' | b'!' | b'[' | b'{')))
        || path.starts_with("..")
}

/// What a search has found so far (gjson's `parseContext`).
#[derive(Default)]
struct Ctx<'a> {
    value: Option<Found<'a>>,
    unsupported: bool,
}

/// `path[start..end]`, or empty when that isn't a range of it.
fn sub(path: &str, start: usize, end: usize) -> &str {
    path.get(start..end).unwrap_or_default()
}

/// `path[start..]`, or empty.
fn tail(path: &str, start: usize) -> &str {
    path.get(start..).unwrap_or_default()
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

fn push(loc: &Loc, step: Step) -> Loc {
    let mut next = loc.clone();
    next.push(step);
    next
}

/// gjson's modifiers, which a `.@name` hands the value to.
const MODIFIERS: [&str; 13] = [
    "pretty", "ugly", "reverse", "this", "flatten", "join", "valid", "keys", "values", "tostr",
    "fromstr", "group", "dig",
];

/// Whether what follows a `.` is a modifier, `[` or `{` (`isDotPiperChar`).
fn is_dot_piper(rest: &[u8]) -> bool {
    match rest.first() {
        Some(b'@') => {
            let end = rest
                .iter()
                .skip(1)
                .position(|&b| matches!(b, b'.' | b'|' | b':'))
                .map_or(rest.len(), |at| at + 1);
            let name = rest.get(1..end).unwrap_or_default();
            MODIFIERS.iter().any(|modifier| modifier.as_bytes() == name)
        }
        Some(b'[' | b'{') => true,
        _ => false,
    }
}

/// The first key of a path and what follows (`objectPathResult`).
#[derive(Default)]
struct ObjectPath {
    part: String,
    path: String,
    piped: bool,
    wild: bool,
    more: bool,
}

/// `parseObjectPath`.
fn parse_object_path(path: &str) -> ObjectPath {
    let bytes = path.as_bytes();
    let mut r = ObjectPath::default();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'|' => {
                r.part = sub(path, 0, i).to_owned();
                r.piped = true;
                return r;
            }
            b'.' => {
                r.part = sub(path, 0, i).to_owned();
                split_after_dot(&mut r, path, i);
                return r;
            }
            b'*' | b'?' => r.wild = true,
            b'\\' => {
                // Escape mode: the escape characters are left out of the
                // part.
                let mut epart = bytes.get(..i).unwrap_or_default().to_vec();
                i += 1;
                if let Some(&escaped) = bytes.get(i) {
                    epart.push(escaped);
                    i += 1;
                    while let Some(&b) = bytes.get(i) {
                        match b {
                            b'\\' => {
                                i += 1;
                                if let Some(&escaped) = bytes.get(i) {
                                    epart.push(escaped);
                                }
                                i += 1;
                                continue;
                            }
                            b'.' => {
                                r.part = text(epart);
                                split_after_dot(&mut r, path, i);
                                return r;
                            }
                            b'|' => {
                                r.part = text(epart);
                                r.piped = true;
                                return r;
                            }
                            b'*' | b'?' => r.wild = true,
                            _ => {}
                        }
                        epart.push(b);
                        i += 1;
                    }
                }
                r.part = text(epart);
                return r;
            }
            _ => {}
        }
        i += 1;
    }
    r.part = path.to_owned();
    r
}

/// After the part ends at the `.` at `dot`: a pipe if a modifier, `[` or
/// `{` follows, else the rest of the path.
fn split_after_dot(r: &mut ObjectPath, path: &str, dot: usize) {
    let rest = tail(path, dot + 1);
    if !rest.is_empty() && is_dot_piper(rest.as_bytes()) {
        r.piped = true;
    } else {
        r.path = rest.to_owned();
        r.more = true;
    }
}

/// A `#(...)` search of an array path.
#[derive(Default)]
struct Query {
    on: bool,
    all: bool,
    bad: bool,
    path: String,
    op: String,
    value: String,
}

/// The first part of a path inside an array and what follows
/// (`arrayPathResult`).
#[derive(Default)]
struct ArrayPath {
    part: String,
    path: String,
    piped: bool,
    more: bool,
    arrch: bool,
    alogok: bool,
    alogkey: String,
    query: Query,
}

/// `parseArrayPath`.
fn parse_array_path(path: &str) -> ArrayPath {
    let bytes = path.as_bytes();
    let mut r = ArrayPath::default();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'|' => {
                r.part = sub(path, 0, i).to_owned();
                r.piped = true;
                return r;
            }
            b'.' => {
                r.part = sub(path, 0, i).to_owned();
                let rest = tail(path, i + 1);
                if !r.arrch && !rest.is_empty() && is_dot_piper(rest.as_bytes()) {
                    r.piped = true;
                } else {
                    r.path = rest.to_owned();
                    r.more = true;
                }
                return r;
            }
            b'#' => {
                r.arrch = true;
                if i == 0 && bytes.len() > 1 {
                    match bytes.get(1) {
                        Some(b'.') => {
                            r.alogok = true;
                            r.alogkey = tail(path, 2).to_owned();
                            r.path = sub(path, 0, 1).to_owned();
                        }
                        Some(b'[' | b'(') => {
                            r.query.on = true;
                            let Some(parsed) = parse_query(tail(path, i)) else {
                                // A bad query ends the path here.
                                r.query.bad = true;
                                break;
                            };
                            let mut value = parsed.value;
                            if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
                                value = sub(value, 1, value.len() - 1);
                                if parsed.vesc {
                                    r.query.value = unescape(value);
                                } else {
                                    r.query.value = value.to_owned();
                                }
                            } else {
                                r.query.value = value.to_owned();
                            }
                            r.query.path = parsed.path.to_owned();
                            r.query.op = parsed.op.to_owned();
                            i = parsed.end;
                            if bytes.get(i) == Some(&b'#') {
                                r.query.all = true;
                            }
                            continue;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    r.part = path.to_owned();
    r.path = String::new();
    r
}

/// A search's parts (`parseQuery`'s results).
struct ParsedQuery<'p> {
    path: &'p str,
    op: &'p str,
    value: &'p str,
    /// Where the search ends: just after its closing bracket.
    end: usize,
    vesc: bool,
}

/// gjson's `trim`: without the bytes up to a space at either end.
fn gtrim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

/// `parseQuery` of `query`, which starts with `#(` or `#[`; `None` if its
/// brackets don't balance.
fn parse_query(query: &str) -> Option<ParsedQuery<'_>> {
    let bytes = query.as_bytes();
    if bytes.len() < 2 || bytes.first() != Some(&b'#') || !matches!(bytes.get(1), Some(b'(' | b'['))
    {
        return None;
    }
    let mut i = 2;
    let mut j = 0;
    let mut depth = 1;
    let mut vesc = false;
    while let Some(&b) = bytes.get(i) {
        if depth == 1 && j == 0 && matches!(b, b'!' | b'=' | b'<' | b'>' | b'%') {
            // The start of the value part.
            j = i;
            i += 1;
            continue;
        }
        match b {
            b'\\' => i += 1,
            b'[' | b'(' => depth += 1,
            b']' | b')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            b'"' => {
                // A string in the selector: its quotes balance.
                i += 1;
                while let Some(&b) = bytes.get(i) {
                    if b == b'\\' {
                        vesc = true;
                        i += 1;
                    } else if b == b'"' {
                        break;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if depth > 0 {
        return None;
    }
    let (path, op, value) = if j > 0 {
        let path = gtrim(sub(query, 2, j));
        let mut value = gtrim(sub(query, j, i));
        let v = value.as_bytes();
        let opsz = match (v.first(), v.get(1)) {
            _ if v.len() == 1 => 1,
            (Some(b'!'), Some(b'=' | b'%')) | (Some(b'<' | b'>'), Some(b'=')) => 2,
            (Some(b'='), Some(b'=')) => {
                value = tail(value, 1);
                1
            }
            (Some(b'<' | b'>' | b'=' | b'%'), _) => 1,
            _ => 0,
        };
        (path, sub(value, 0, opsz), gtrim(tail(value, opsz)))
    } else {
        (gtrim(sub(query, 2, i)), "", "")
    };
    Some(ParsedQuery {
        path,
        op,
        value,
        end: i + 1,
        vesc,
    })
}

/// `parseObject`: looks for `path` in `map`, at `loc`. Returns whether the
/// search is over.
fn parse_object<'a>(ctx: &mut Ctx<'a>, map: &'a Map<String, Value>, path: &str, loc: &Loc) -> bool {
    let rp = parse_object_path(path);
    if rp.piped {
        ctx.unsupported = true;
        return true;
    }
    for (key, value) in map {
        let pmatch = if rp.wild {
            match_limit(key, &rp.part)
        } else {
            *key == rp.part
        };
        if !pmatch {
            continue;
        }
        let hit = !rp.more;
        let at = push(loc, Step::Key(key.clone()));
        match value {
            Value::Object(inner) if !hit => {
                if parse_object(ctx, inner, &rp.path, &at) {
                    return true;
                }
            }
            Value::Array(items) if !hit => {
                if parse_array(ctx, items, &rp.path, &at) {
                    return true;
                }
            }
            _ if hit => {
                ctx.value = Some(Found::At(value, at));
                return true;
            }
            _ => {}
        }
    }
    false
}

/// gjson's `parseUint`: the digits of `s` as a number, wrapping, or `None`
/// if `s` is empty or isn't all digits.
pub(super) fn parse_uint(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.bytes().fold(0u64, |n, b| {
        n.wrapping_mul(10).wrapping_add(u64::from(b - b'0'))
    }))
}

/// `parseArray`: looks for `path` in `items`, at `loc`. Returns whether the
/// search is over.
fn parse_array<'a>(ctx: &mut Ctx<'a>, items: &'a [Value], path: &str, loc: &Loc) -> bool {
    let rp = parse_array_path(path);
    if rp.piped || rp.query.bad {
        ctx.unsupported = true;
        return true;
    }
    // Go converts the index to an int, so a huge one wraps.
    #[allow(clippy::cast_possible_wrap)]
    let partidx = if rp.arrch {
        None
    } else {
        parse_uint(&rp.part).map(|n| n as i64)
    };
    let mut multires = Vec::new();
    for (h, item) in items.iter().enumerate() {
        let pmatch = !rp.arrch && i64::try_from(h).ok() == partidx && partidx.is_some();
        let hit = pmatch && !rp.more;
        let at = push(loc, Step::Index(h));
        let over = match item {
            Value::Object(inner) if pmatch && !hit => parse_object(ctx, inner, &rp.path, &at),
            Value::Array(inner) if pmatch && !hit => parse_array(ctx, inner, &rp.path, &at),
            _ if rp.query.on => proc_query(ctx, &rp, item, at, &mut multires),
            _ if hit => {
                ctx.value = Some(Found::At(item, at));
                true
            }
            _ => false,
        };
        if over || ctx.unsupported {
            return true;
        }
    }
    if rp.arrch && rp.part == "#" {
        if rp.alogok {
            let mut found = Vec::new();
            for (h, item) in items.iter().enumerate() {
                match get_from(item, &rp.alogkey, &push(loc, Step::Index(h))) {
                    Ok(Some(value)) => found.push(value),
                    Ok(None) => {}
                    Err(Unsupported) => {
                        ctx.unsupported = true;
                        return true;
                    }
                }
            }
            ctx.value = Some(Found::List(found, true));
            return true;
        }
        ctx.value = Some(Found::Count(items.len()));
        return true;
    }
    if ctx.value.is_none() && rp.query.all {
        ctx.value = Some(Found::List(multires, true));
    }
    false
}

/// A search's test of one array item, at `at` (`procQuery`). Returns
/// whether the search is over.
fn proc_query<'a>(
    ctx: &mut Ctx<'a>,
    rp: &ArrayPath,
    item: &'a Value,
    at: Loc,
    multires: &mut Vec<Found<'a>>,
) -> bool {
    let res = match item {
        Value::Object(_) | Value::Array(_) => match get_from(item, &rp.query.path, &at) {
            Ok(res) => res,
            Err(Unsupported) => {
                ctx.unsupported = true;
                return true;
            }
        },
        _ if !rp.query.path.is_empty() => return false,
        _ => Some(Found::At(item, at.clone())),
    };
    if !query_matches(&rp.query, res.as_ref()) {
        return false;
    }
    let res = if rp.more {
        match get_from(item, &rp.path, &at) {
            // Upstream's offsets for a list found here are relative.
            Ok(Some(Found::List(items, _))) if !rp.query.all => Some(Found::List(items, false)),
            Ok(res) => res,
            Err(Unsupported) => {
                ctx.unsupported = true;
                return true;
            }
        }
    } else {
        Some(Found::At(item, at))
    };
    if rp.query.all {
        multires.extend(res);
        false
    } else {
        ctx.value = res;
        true
    }
}

/// A value's gjson type, with what its comparisons read.
enum Kind<'v> {
    Null,
    False,
    True,
    Number(f64),
    String(&'v str),
    Json,
}

fn kind<'v>(found: &'v Found<'_>) -> Kind<'v> {
    match found {
        Found::At(Value::Null, _) => Kind::Null,
        Found::At(Value::Bool(false), _) => Kind::False,
        Found::At(Value::Bool(true), _) => Kind::True,
        Found::At(Value::Number(number), _) => Kind::Number(parse_float(number.as_str())),
        Found::At(Value::String(text), _) => Kind::String(text),
        Found::At(_, _) | Found::List(..) => Kind::Json,
        #[allow(clippy::cast_precision_loss)]
        Found::Count(count) => Kind::Number(*count as f64),
    }
}

/// `strconv.ParseBool` of `strings.ToLower(text)`.
fn parse_bool(text: &str) -> Option<bool> {
    match to_lower(text).as_str() {
        "1" | "t" | "true" => Some(true),
        "0" | "f" | "false" => Some(false),
        _ => None,
    }
}

/// `trueish`.
fn trueish(found: Option<&Found<'_>>) -> bool {
    match found.map(kind) {
        Some(Kind::True) => true,
        Some(Kind::String(text)) => parse_bool(text) == Some(true),
        Some(Kind::Number(number)) => number != 0.0,
        _ => false,
    }
}

/// `falseish`: what isn't there is null.
fn falseish(found: Option<&Found<'_>>) -> bool {
    match found.map(kind) {
        None | Some(Kind::Null | Kind::False) => true,
        Some(Kind::String(text)) => parse_bool(text) == Some(false),
        Some(Kind::Number(number)) => number == 0.0,
        _ => false,
    }
}

/// `nullish`: what isn't there is null.
fn nullish(found: Option<&Found<'_>>) -> bool {
    matches!(found.map(kind), None | Some(Kind::Null))
}

/// `queryMatches`: whether a search's `value` passes its comparison.
fn query_matches(query: &Query, value: Option<&Found<'_>>) -> bool {
    let mut rpv = query.value.as_str();
    let mut kind = value.map(kind);
    if let Some(name) = rpv.strip_prefix('~') {
        // Converted to a boolean.
        let ish = match name {
            "*" => Some(value.is_some()),
            "null" => Some(nullish(value)),
            "true" => Some(trueish(value)),
            "false" => Some(falseish(value)),
            _ => None,
        };
        match ish {
            Some(ish) => {
                rpv = "true";
                kind = Some(if ish { Kind::True } else { Kind::False });
            }
            None => {
                rpv = "";
                kind = None;
            }
        }
    }
    let Some(kind) = kind else {
        return false;
    };
    let op = query.op.as_str();
    if op.is_empty() {
        // Only whether it exists.
        return true;
    }
    match kind {
        Kind::String(text) => match op {
            "=" => text == rpv,
            "!=" => text != rpv,
            "<" => text < rpv,
            "<=" => text <= rpv,
            ">" => text > rpv,
            ">=" => text >= rpv,
            "%" => match_limit(text, rpv),
            "!%" => !match_limit(text, rpv),
            _ => false,
        },
        Kind::Number(number) => {
            let rpvn = parse_float(rpv);
            match op {
                "=" => number == rpvn,
                "!=" => number != rpvn,
                "<" => number < rpvn,
                "<=" => number <= rpvn,
                ">" => number > rpvn,
                ">=" => number >= rpvn,
                _ => false,
            }
        }
        Kind::True => match op {
            "=" => rpv == "true",
            "!=" => rpv != "true",
            ">" => rpv == "false",
            ">=" => true,
            _ => false,
        },
        Kind::False => match op {
            "=" => rpv == "false",
            "!=" => rpv != "false",
            "<" => rpv == "true",
            "<=" => true,
            _ => false,
        },
        Kind::Null | Kind::Json => false,
    }
}

/// gjson's `unescape` of a string's text: stops at a control character or
/// a bad escape, and writes U+FFFD for a lone surrogate.
pub(super) fn unescape(json: &str) -> String {
    let bytes = json.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b < b' ' {
            break;
        }
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&escaped) = bytes.get(i) else {
            break;
        };
        match escaped {
            b'\\' | b'/' | b'"' => out.push(escaped),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                if i + 5 > bytes.len() {
                    break;
                }
                let mut code = hex4(bytes.get(i + 1..i + 5).unwrap_or_default());
                i += 5;
                if (0xd800..0xe000).contains(&code)
                    && bytes.len() - i >= 6
                    && bytes.get(i) == Some(&b'\\')
                    && bytes.get(i + 1) == Some(&b'u')
                {
                    let low = hex4(bytes.get(i + 2..i + 6).unwrap_or_default());
                    code = if (0xd800..0xdc00).contains(&code) && (0xdc00..0xe000).contains(&low) {
                        0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00)
                    } else {
                        0xfffd
                    };
                    i += 6;
                }
                let c = char::from_u32(code).unwrap_or('\u{fffd}');
                let mut buf = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                continue;
            }
            _ => break,
        }
        i += 1;
    }
    text(out)
}

/// `strconv.ParseUint(hex, 16, 64)` of four bytes, 0 if they aren't all hex
/// digits.
fn hex4(hex: &[u8]) -> u32 {
    std::str::from_utf8(hex)
        .ok()
        .filter(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()))
        .and_then(|digits| u32::from_str_radix(digits, 16).ok())
        .unwrap_or(0)
}

/// How a match ended (`match`'s result).
#[derive(PartialEq, Eq)]
enum Outcome {
    NoMatch,
    Match,
    Stop,
}

/// tidwall/match's `MatchLimit(s, pattern, 10000)`: whether `s` matches
/// `pattern`, where `*` matches any characters, `?` one and `\` escapes
/// the next, giving up as no match after 10000 steps per byte of `s`.
pub(super) fn match_limit(s: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let mut counter = 0;
    match_from(s, pattern, s.len(), &mut counter, 10_000) == Outcome::Match
}

fn match_from(
    mut s: &str,
    mut pat: &str,
    slen: usize,
    counter: &mut usize,
    maxcomp: usize,
) -> Outcome {
    if *counter > slen.saturating_mul(maxcomp) {
        return Outcome::Stop;
    }
    *counter += 1;
    while let Some(pc) = pat.chars().next() {
        let mut ps = pc.len_utf8();
        let sc = s.chars().next();
        let ss = sc.map_or(0, char::len_utf8);
        let mut wild = false;
        match pc {
            '?' => {
                if ss == 0 {
                    return Outcome::NoMatch;
                }
            }
            '*' => {
                // Repeated stars are one.
                while pat.as_bytes().get(1) == Some(&b'*') {
                    pat = tail(pat, 1);
                }
                // A last star matches the rest.
                if pat.len() == 1 {
                    return Outcome::Match;
                }
                let (trimmed, rest, ok) = match_trim_suffix(s, pat);
                s = trimmed;
                pat = rest;
                if !ok {
                    return Outcome::NoMatch;
                }
                if pat.len() == 1 {
                    return Outcome::Match;
                }
                let r = match_from(s, tail(pat, 1), slen, counter, maxcomp);
                if r != Outcome::NoMatch {
                    return r;
                }
                if s.is_empty() {
                    return Outcome::NoMatch;
                }
                wild = true;
            }
            _ => {
                if ss == 0 {
                    return Outcome::NoMatch;
                }
                let mut pc = pc;
                if pc == '\\' {
                    pat = tail(pat, ps);
                    let Some(escaped) = pat.chars().next() else {
                        return Outcome::NoMatch;
                    };
                    pc = escaped;
                    ps = escaped.len_utf8();
                }
                if Some(pc) != sc {
                    return Outcome::NoMatch;
                }
            }
        }
        s = tail(s, ss);
        if !wild {
            pat = tail(pat, ps);
        }
    }
    if s.is_empty() {
        Outcome::Match
    } else {
        Outcome::NoMatch
    }
}

/// `matchTrimSuffix`: matches the characters after the last star of `pat`,
/// which starts with a star, against the end of `s`, and trims them from
/// both.
fn match_trim_suffix<'s, 'p>(mut s: &'s str, mut pat: &'p str) -> (&'s str, &'p str, bool) {
    let mut matched = true;
    while !s.is_empty() && pat.len() > 1 {
        let Some(pc) = pat.chars().next_back() else {
            break;
        };
        let mut ps = pc.len_utf8();
        let bytes = pat.as_bytes();
        let mut esc = false;
        let mut i = 0;
        loop {
            let before = bytes
                .len()
                .checked_sub(ps + i + 1)
                .and_then(|at| bytes.get(at));
            if before == Some(&b'\\') {
                i += 1;
                continue;
            }
            if i & 1 == 1 {
                esc = true;
                ps += 1;
            }
            break;
        }
        if pc == '*' && !esc {
            matched = true;
            break;
        }
        let sc = s.chars().next_back();
        if !((pc == '?' && !esc) || Some(pc) == sc) {
            matched = false;
            break;
        }
        let ss = sc.map_or(0, char::len_utf8);
        s = sub(s, 0, s.len() - ss);
        pat = sub(pat, 0, pat.len().saturating_sub(ps));
    }
    (s, pat, matched)
}
