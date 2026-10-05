// Ported from tidwall/sjson v1.2.5 sjson.go (isSimpleChar, parsePath,
// appendBuild, atoui, appendRawPaths, set, setComplexPath) (MIT), as
// CLIProxyAPI uses it in internal/runtime/executor/helps/payload_helpers.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/tidwall/sjson

//! sjson's `SetBytes`, `SetRawBytes` and `DeleteBytes` over a parsed body.
//!
//! A simple path (no `|`, `#`, `@`, `*` or `?` outside an escape) is
//! walked a key or index at a time: a value found is replaced or removed,
//! and what is missing is built, an array for a numeric key and an object
//! otherwise. A `:` before a key makes it an object key even when it is
//! numeric; `-1` appends to an array. A complex path replaces each value
//! [`super::gjson::get`] finds and can't remove anything.
//!
//! Deviations from upstream:
//! - A key starting with `!`, `[` or `{` is looked up as it is written,
//!   where sjson reads it as gjson's literal or multipath syntax.
//! - `-1` removes an array's last item; an object's key `-1` is removed
//!   like any other, where sjson first looks for a key `#`.
//! - Filling an array up to an index more than 1024 past its end is
//!   refused, where sjson writes as many `null`s as it takes. A refused
//!   write changes nothing, as in sjson, which returns the error and the
//!   document it was given.
//! - A set of a simple path of more than 64 keys is refused, where sjson
//!   builds a path of any length. A path is built a key at a time, and
//!   a value 2,000 levels deep takes more stack than a thread has to build,
//!   write and drop. Deleting a path is not limited: the walk stops where
//!   the document does.
//! - A complex path whose results upstream can't place, or places wrongly
//!   (a projection inside another, a count, or a list found through a
//!   `#(query)` with a path after it), changes nothing, where sjson writes
//!   the value over whatever bytes its offsets point at.

use serde_json::{Map, Value};

use super::gjson::{self, Found, Loc, Step};

/// The most `null`s a set writes to reach an index past an array's end.
const MAX_PADDING: i64 = 1024;

/// The most keys a simple path may have for a set.
pub(super) const MAX_KEYS: usize = 64;

/// Whether a set of `path` is over the [`MAX_KEYS`] limit, counting a key
/// for each `.` and one more. That's a bound on the keys of the simple path
/// the rules' `path::resolve` makes of `path` (a `#(query)` that may hold
/// dots becomes one index), never an undercount.
pub(super) fn too_deep(path: &str) -> bool {
    path.bytes().filter(|&b| b == b'.').count() >= MAX_KEYS
}

/// Why a set or delete failed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SetError {
    /// `path cannot be empty`.
    EmptyPath,
    /// `cannot delete value from a complex path`.
    ComplexDelete,
    /// `cannot set array element for non-numeric key`.
    NonNumericKey,
    /// An index too far past an array's end (not upstream's).
    TooFar,
    /// A path of more than [`MAX_KEYS`] keys to set (not upstream's).
    TooDeep,
}

/// What a walk found nothing to do for (sjson's `errNoChange`), which the
/// public functions report as success.
enum Outcome {
    Done,
    NoChange,
}

/// Sets `path` in `doc` to `value` (`sjson.SetBytes` or `SetRawBytes`).
pub(super) fn set(doc: &mut Value, path: &str, value: &Value) -> Result<(), SetError> {
    run(doc, path, Some(value)).map(|_| ())
}

/// Removes `path` from `doc` (`sjson.DeleteBytes`). A path that isn't
/// there is no error.
pub(super) fn delete(doc: &mut Value, path: &str) -> Result<(), SetError> {
    run(doc, path, None).map(|_| ())
}

fn run(doc: &mut Value, path: &str, value: Option<&Value>) -> Result<Outcome, SetError> {
    if path.is_empty() {
        return Err(SetError::EmptyPath);
    }
    match parse_components(path) {
        Some(components) => {
            if value.is_some() && components.len() > MAX_KEYS {
                return Err(SetError::TooDeep);
            }
            append_paths(doc, &components, value)
        }
        None => match value {
            Some(value) => Ok(set_complex(doc, path, value)),
            None => Err(SetError::ComplexDelete),
        },
    }
}

/// One key of a simple path (`pathResult`).
#[derive(Debug)]
struct Component {
    /// The key without its escapes.
    part: String,
    /// The key as gjson reads it, escapes kept.
    gpart: String,
    /// Whether a `:` made it an object key.
    force: bool,
}

/// Whether sjson takes `b` as part of a key (`isSimpleChar`).
fn is_simple(b: u8) -> bool {
    !matches!(b, b'|' | b'#' | b'@' | b'*' | b'?')
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// The keys of `path`, or `None` if it's a complex path (`parsePath` over
/// each key).
fn parse_components(path: &str) -> Option<Vec<Component>> {
    let mut components = Vec::new();
    let mut rest = path;
    loop {
        let (component, more) = parse_path(rest)?;
        components.push(component);
        match more {
            Some(next) => rest = next,
            None => return Some(components),
        }
    }
}

/// The first key of `path` and what follows its `.`, or `None` if it isn't
/// simple (`parsePath`).
fn parse_path(path: &str) -> Option<(Component, Option<&str>)> {
    let (force, path) = match path.strip_prefix(':') {
        Some(rest) => (true, rest),
        None => (false, path),
    };
    let bytes = path.as_bytes();
    let rest_after = |i: usize| Some(path.get(i + 1..).unwrap_or_default());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'.' {
            let part = path.get(..i).unwrap_or_default().to_owned();
            let component = Component {
                gpart: part.clone(),
                part,
                force,
            };
            return Some((component, rest_after(i)));
        }
        if !is_simple(b) {
            return None;
        }
        if b == b'\\' {
            // Escape mode: the escapes are left out of the part and kept in
            // what gjson reads.
            let mut epart = bytes.get(..i).unwrap_or_default().to_vec();
            let mut gpart = bytes.get(..=i).unwrap_or_default().to_vec();
            i += 1;
            if let Some(&escaped) = bytes.get(i) {
                epart.push(escaped);
                gpart.push(escaped);
                i += 1;
                while let Some(&b) = bytes.get(i) {
                    if b == b'\\' {
                        gpart.push(b'\\');
                        i += 1;
                        if let Some(&escaped) = bytes.get(i) {
                            epart.push(escaped);
                            gpart.push(escaped);
                        }
                        i += 1;
                        continue;
                    }
                    if b == b'.' {
                        let component = Component {
                            part: text(epart),
                            gpart: text(gpart),
                            force,
                        };
                        return Some((component, rest_after(i)));
                    }
                    if !is_simple(b) {
                        return None;
                    }
                    epart.push(b);
                    gpart.push(b);
                    i += 1;
                }
            }
            let component = Component {
                part: text(epart),
                gpart: text(gpart),
                force,
            };
            return Some((component, None));
        }
        i += 1;
    }
    let component = Component {
        part: path.to_owned(),
        gpart: path.to_owned(),
        force,
    };
    Some((component, None))
}

/// A key as an array index (`atoui`): its digits as a number, wrapping as
/// Go's `int` does, and whether it is one. An empty key is index 0; a
/// forced one is no index.
fn atoui(component: &Component) -> (i64, bool) {
    if component.force {
        return (0, false);
    }
    let mut n: i64 = 0;
    for b in component.part.bytes() {
        if !b.is_ascii_digit() {
            return (0, false);
        }
        n = n.wrapping_mul(10).wrapping_add(i64::from(b - b'0'));
    }
    (n, true)
}

/// Whether the component appends to an array (`-1`, not forced).
fn is_append(component: &Component) -> bool {
    !component.force && component.part == "-1"
}

/// `count` `null`s, refusing more than [`MAX_PADDING`].
fn padding(count: i64) -> Result<Vec<Value>, SetError> {
    if count > MAX_PADDING {
        return Err(SetError::TooFar);
    }
    let count = usize::try_from(count.max(0)).unwrap_or_default();
    Ok(vec![Value::Null; count])
}

/// What goes where `components`' first key is missing (`appendBuild`):
/// the value, or the containers the remaining keys name, built around it.
fn build(components: &[Component], value: &Value) -> Result<Value, SetError> {
    let Some(next) = components.get(1) else {
        return Ok(value.clone());
    };
    let inner = build(components.get(1..).unwrap_or_default(), value)?;
    let (n, numeric) = atoui(next);
    if numeric || is_append(next) {
        let mut items = padding(n)?;
        items.push(inner);
        Ok(Value::Array(items))
    } else {
        let mut map = Map::new();
        map.insert(next.part.clone(), inner);
        Ok(Value::Object(map))
    }
}

/// Where a component is in `node`, as gjson finds its `gpart` there (and,
/// for a delete, sjson's `-1` for an array's last item).
enum Slot {
    Key(String),
    Index(usize),
}

fn find(node: &Value, component: &Component, delete: bool) -> Option<Slot> {
    match node {
        Value::Object(map) => map
            .contains_key(&component.part)
            .then(|| Slot::Key(component.part.clone())),
        Value::Array(items) => {
            if delete && is_append(component) && !items.is_empty() {
                return Some(Slot::Index(items.len() - 1));
            }
            // gjson reads the index as a wrapping uint64 converted to int.
            #[allow(clippy::cast_possible_wrap)]
            let index = gjson::parse_uint(&component.gpart)? as i64;
            usize::try_from(index)
                .ok()
                .filter(|&index| index < items.len())
                .map(Slot::Index)
        }
        _ => None,
    }
}

/// `appendRawPaths`: sets (or, for a `None` value, removes) the path of
/// `components` in `node`.
fn append_paths(
    node: &mut Value,
    components: &[Component],
    value: Option<&Value>,
) -> Result<Outcome, SetError> {
    let Some(component) = components.first() else {
        return Ok(Outcome::NoChange);
    };
    let more = components.len() > 1;
    if let Some(slot) = find(node, component, value.is_none()) {
        let child = match (&slot, &mut *node) {
            (Slot::Key(key), Value::Object(map)) => map.get_mut(key),
            (Slot::Index(index), Value::Array(items)) => items.get_mut(*index),
            _ => None,
        };
        let Some(child) = child else {
            return Ok(Outcome::NoChange);
        };
        if more {
            return append_paths(child, components.get(1..).unwrap_or_default(), value);
        }
        match value {
            Some(value) => *child = value.clone(),
            None => match (slot, node) {
                (Slot::Key(key), Value::Object(map)) => {
                    map.shift_remove(&key);
                }
                (Slot::Index(index), Value::Array(items)) => {
                    items.remove(index);
                }
                _ => {}
            },
        }
        return Ok(Outcome::Done);
    }
    let Some(value) = value else {
        return Ok(Outcome::NoChange);
    };
    let (n, numeric) = atoui(component);
    // Everything that can fail is done before `node` changes, so a refused
    // write leaves the document as it was.
    let built = build(components, value)?;
    match node {
        Value::Object(map) => {
            map.insert(component.part.clone(), built);
        }
        Value::Array(items) => {
            if numeric {
                let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
                items.extend(padding(n.saturating_sub(len))?);
            } else if !is_append(component) {
                return Err(SetError::NonNumericKey);
            }
            items.push(built);
        }
        // A scalar is replaced by the container the key names.
        scalar => {
            *scalar = if numeric {
                let mut items = padding(n)?;
                items.push(built);
                Value::Array(items)
            } else {
                let mut map = Map::new();
                map.insert(component.part.clone(), built);
                Value::Object(map)
            };
        }
    }
    Ok(Outcome::Done)
}

/// `setComplexPath`: writes `value` over each value gjson finds at `path`.
fn set_complex(doc: &mut Value, path: &str, value: &Value) -> Outcome {
    let locs: Vec<Loc> = match gjson::get(doc, path) {
        // The document itself is at offset 0, which sjson takes as nothing
        // found.
        Some(Found::At(_, loc)) if !loc.is_empty() => vec![loc],
        Some(Found::List(items, true)) => {
            let mut locs = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Found::At(_, loc) if !loc.is_empty() => locs.push(loc),
                    _ => return Outcome::NoChange,
                }
            }
            locs
        }
        _ => return Outcome::NoChange,
    };
    if locs.is_empty() {
        return Outcome::NoChange;
    }
    for loc in &locs {
        if let Some(slot) = locate_mut(doc, loc) {
            *slot = value.clone();
        }
    }
    Outcome::Done
}

/// The value at `loc` in `doc`.
fn locate_mut<'a>(doc: &'a mut Value, loc: &[Step]) -> Option<&'a mut Value> {
    loc.iter().try_fold(doc, |node, step| match (step, node) {
        (Step::Key(key), Value::Object(map)) => map.get_mut(key),
        (Step::Index(index), Value::Array(items)) => items.get_mut(*index),
        _ => None,
    })
}
