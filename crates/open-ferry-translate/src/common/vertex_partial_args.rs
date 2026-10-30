// Ported from CLIProxyAPI internal/translator/common/vertex_partial_args.go
// (64316127, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Bounded Vertex streaming function-call arguments, emitted as concatenable JSON.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io;

use serde_json::Value;

const MAX_DEPTH: usize = 64;
const MAX_INDEX: usize = 1024;
const MAX_BYTES: usize = 4 * 1024 * 1024;
// A single buffered raw string can expand sixfold when quoted. Emitted output
// is still limited to MAX_BYTES; pre-clean snapshots are not emitted output.
const MAX_QUOTED_BYTES: usize = 6 * MAX_BYTES + 2;

#[derive(Clone, Copy)]
pub(crate) enum ProbeStep<'a> {
    Key(&'a str),
    Index,
}
type Probe<'a> = Option<&'a dyn Fn(usize, ProbeStep<'_>) -> Option<(bool, usize)>>;

type Cleaner<'a> = Option<&'a dyn Fn(&Value) -> Value>;
type Result<T> = std::result::Result<T, ArgumentError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArgumentError(&'static str);

impl fmt::Display for ArgumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ArgumentError {}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum Segment {
    Key(String),
    Index(usize),
}

#[derive(Default)]
enum Kind {
    #[default]
    Unset,
    Object(ObjectChildren),
    Array(BTreeMap<usize, usize>),
    String {
        escaped: String,
        open: bool,
    },
    Scalar(String),
}

// Upstream keeps keyed children and a separate emission-order list.
#[derive(Default)]
struct ObjectChildren {
    by_key: HashMap<String, usize>,
    order: Vec<(String, usize)>,
}

#[derive(Default)]
struct Node {
    kind: Kind,
    probe_state: usize,
    held: bool,
    opened: bool,
    closed: bool,
    next: usize,
    entry_opened: bool,
}

/// One tree per call. Containers remain open until the call ends, allowing
/// non-depth-first updates and strings interleaved with later fields.
#[derive(Default)]
pub(crate) struct Assembler {
    nodes: Vec<Node>,
    root: Option<usize>,
    last_string: Option<usize>,
    probes: HashMap<Vec<Segment>, bool>,
    buffered: usize,
    #[cfg(test)]
    lookup_steps: std::cell::Cell<usize>,
    full: String,
    error: Option<ArgumentError>,
    finished: bool,
}

impl Assembler {
    pub(crate) fn push(&mut self, call: &Value, cleaner: Cleaner<'_>) -> Result<(String, bool)> {
        self.push_indexed(call, cleaner, None)
    }

    pub(crate) fn push_indexed(
        &mut self,
        call: &Value,
        cleaner: Cleaner<'_>,
        probe: Probe<'_>,
    ) -> Result<(String, bool)> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let result = self.push_inner(call, cleaner, probe);
        if let Err(error) = result {
            self.error = Some(error);
            // Never expose a partially valid final argument object after failure.
            self.full.clear();
        }
        result
    }

    fn push_inner(
        &mut self,
        call: &Value,
        cleaner: Cleaner<'_>,
        probe: Probe<'_>,
    ) -> Result<(String, bool)> {
        if self.finished {
            return Err(ArgumentError("Vertex arguments received after completion"));
        }
        let start = self.full.len();
        let final_chunk = !will_continue(call);
        if let Some(parts) = field(call, "partialArgs", "partial_args") {
            let parts = parts
                .as_array()
                .ok_or(ArgumentError("Vertex partialArgs must be an array"))?;
            for part in parts {
                self.push_part(part, cleaner, probe)?;
            }
        }
        if final_chunk && self.root.is_none() {
            let empty = serde_json::json!({});
            let args = call
                .get("args")
                .filter(|args| args.is_object())
                .unwrap_or(&empty);
            self.full = match cleaner {
                Some(clean) => bounded_json(&clean(args))?,
                None => bounded_json(args)?,
            };
        } else if let Some(root) = self.root {
            if final_chunk && let Some(clean) = cleaner {
                let mut raw = String::new();
                self.serialize(root, &mut raw)?;
                let value = crate::json::exact::from_str(&raw)
                    .map_err(|_| ArgumentError("invalid assembled Vertex arguments"))?;
                let cleaned = clean(&value);
                self.prune(root, &cleaned);
            }
            self.emit(root, final_chunk)?;
        }
        self.finished = final_chunk;
        Ok((self.full[start..].to_owned(), final_chunk))
    }

    pub(crate) fn full_arguments(&self) -> Result<&str> {
        match self.error {
            Some(error) => Err(error),
            None if self.full.is_empty() => Ok("{}"),
            None => Ok(&self.full),
        }
    }

    fn reserve(&mut self, bytes: usize) -> Result<()> {
        self.buffered = self.buffered.saturating_add(bytes);
        if self.buffered > MAX_BYTES {
            return Err(ArgumentError("Vertex argument size limit exceeded"));
        }
        Ok(())
    }

    fn alloc(&mut self, kind: Kind, held: bool) -> Result<usize> {
        let index = self.nodes.len();
        self.nodes.push(Node {
            kind,
            held,
            ..Node::default()
        });
        Ok(index)
    }

    fn object_child(&self, children: &ObjectChildren, key: &str) -> Option<usize> {
        #[cfg(test)]
        self.lookup_steps
            .set(self.lookup_steps.get() + 1);
        children.by_key.get(key).copied()
    }

    fn find(&self, segments: &[Segment]) -> Result<Option<usize>> {
        let Some(mut current) = self.root else {
            return Ok(None);
        };
        for segment in segments {
            let child = match (&self.nodes[current].kind, segment) {
                (Kind::Object(children), Segment::Key(key)) => self.object_child(children, key),
                (Kind::Array(children), Segment::Index(index)) => children.get(index).copied(),
                _ => None,
            };
            let Some(child) = child else {
                return Ok(None);
            };
            current = child;
        }
        Ok(Some(current))
    }

    fn push_part(&mut self, part: &Value, cleaner: Cleaner<'_>, probe: Probe<'_>) -> Result<()> {
        if !part.is_object() {
            return Err(ArgumentError(
                "Vertex partialArgs element must be an object",
            ));
        }
        let path = field(part, "jsonPath", "json_path");
        let raw_path = crate::json::str_of(path);
        let segments = parse_path(&raw_path)?;
        let string = field(part, "stringValue", "string_value").filter(|v| !v.is_null());
        let number = field(part, "numberValue", "number_value").filter(|v| !v.is_null());
        let boolean = field(part, "boolValue", "bool_value").filter(|v| !v.is_null());
        let null = field(part, "nullValue", "null_value").is_some();
        let continuing = will_continue(part);
        if string.is_none() && number.is_none() && boolean.is_none() && !null && !continuing {
            let node = if segments.is_empty() {
                self.last_string
            } else {
                self.find(&segments)?
            };
            if let Some(node) = node
                && let Kind::String { open, .. } = &mut self.nodes[node].kind
            {
                *open = false;
            }
            return Ok(());
        }
        if segments.is_empty() {
            return Err(ArgumentError("Vertex value requires a non-empty JSON path"));
        }
        let root = match self.root {
            Some(root) => root,
            None => {
                let root = self.alloc(container(&segments[0]), false)?;
                self.root = Some(root);
                root
            }
        };
        let mut current = root;
        for (at, segment) in segments.iter().enumerate() {
            if self.nodes[current].closed {
                return Err(ArgumentError("cannot modify completed Vertex container"));
            }
            let found = match (&self.nodes[current].kind, segment) {
                (Kind::Object(children), Segment::Key(key)) => self.object_child(children, key),
                (Kind::Array(children), Segment::Index(index)) => children.get(index).copied(),
                _ => return Err(ArgumentError("conflicting Vertex container types")),
            };
            let child = match found {
                Some(child) => child,
                None => {
                    let indexed = probe.and_then(|query| {
                        query(
                            self.nodes[current].probe_state,
                            match segment {
                                Segment::Key(key) => ProbeStep::Key(key),
                                Segment::Index(_) => ProbeStep::Index,
                            },
                        )
                    });
                    let held = match segment {
                        Segment::Key(key) => {
                            self.reserve(key.len())?;
                            match indexed {
                                Some((held, _)) => held,
                                None => self.should_hold(&segments[..at], key, cleaner)?,
                            }
                        }
                        Segment::Index(_) => false,
                    };
                    let kind = segments.get(at + 1).map_or(Kind::Unset, container);
                    self.reserve(4)?;
                    let child = self.alloc(kind, held)?;
                    self.nodes[child].probe_state = indexed.map_or(0, |(_, state)| state);
                    let next = self.nodes[current].next;
                    let insert = match &self.nodes[current].kind {
                        Kind::Object(children) if !held => children
                            .order
                            .iter()
                            .enumerate()
                            .skip(next)
                            .find(|(_, (_, id))| self.nodes[*id].held)
                            .map_or(children.order.len(), |(at, _)| at),
                        Kind::Object(children) => children.order.len(),
                        _ => 0,
                    };
                    match (&mut self.nodes[current].kind, segment) {
                        (Kind::Object(children), Segment::Key(key)) => {
                            children.by_key.insert(key.clone(), child);
                            children.order.insert(insert, (key.clone(), child));
                        }
                        (Kind::Array(children), Segment::Index(index)) => {
                            children.insert(*index, child);
                        }
                        _ => unreachable!(),
                    }
                    child
                }
            };
            current = child;
        }
        let leaf = &mut self.nodes[current];
        if string.is_some() || continuing {
            match &leaf.kind {
                Kind::Unset => {}
                Kind::String { open: true, .. } if !leaf.closed => {}
                _ => return Err(ArgumentError("cannot append to completed Vertex value")),
            }
            let value = crate::json::str_of(string);
            if value.len() > MAX_BYTES {
                return Err(ArgumentError("Vertex argument size limit exceeded"));
            }
            self.reserve(value.len())?;
            let quoted = bounded_quoted(&value)?;
            let escaped = &quoted[1..quoted.len() - 1];
            let leaf = &mut self.nodes[current];
            if matches!(leaf.kind, Kind::Unset) {
                leaf.kind = Kind::String {
                    escaped: String::new(),
                    open: true,
                };
            }
            if let Kind::String {
                escaped: buffer,
                open,
            } = &mut leaf.kind
            {
                buffer.push_str(escaped);
                *open = continuing;
            }
            self.last_string = Some(current);
        } else {
            if !matches!(leaf.kind, Kind::Unset) {
                return Err(ArgumentError("cannot overwrite completed Vertex value"));
            }
            let scalar = if let Some(number) = number {
                let raw = number_text(number)?;
                self.reserve(raw.len())?;
                raw
            } else if let Some(boolean) = boolean {
                crate::json::bool_of(boolean).to_string()
            } else {
                "null".to_owned()
            };
            self.nodes[current].kind = Kind::Scalar(scalar);
        }
        Ok(())
    }

    fn should_hold(&mut self, path: &[Segment], key: &str, cleaner: Cleaner<'_>) -> Result<bool> {
        let Some(clean) = cleaner else {
            return Ok(false);
        };
        let probe_key = if matches!(key, "reason" | "_") {
            key
        } else {
            "__cpa_probe__"
        };
        let mut cache_key = path.to_vec();
        // An array's schema is the same at every index.
        for segment in &mut cache_key {
            if let Segment::Index(index) = segment {
                *index = 0;
            }
        }
        cache_key.push(Segment::Key(probe_key.to_owned()));
        if let Some(held) = self.probes.get(&cache_key) {
            return Ok(*held);
        }
        // Probe paths are not buffered argument bytes.
        let mut probe = serde_json::json!({probe_key:1});
        for segment in path.iter().rev() {
            probe = match segment {
                Segment::Key(key) => {
                    let mut object = serde_json::Map::new();
                    object.insert(key.clone(), probe);
                    Value::Object(object)
                }
                Segment::Index(_) => Value::Array(vec![probe]),
            };
        }
        let cleaned = clean(&probe);
        let mut current = Some(&cleaned);
        for segment in path {
            current = current.and_then(|v| match segment {
                Segment::Key(key) => v.get(key),
                Segment::Index(_) => v.get(0),
            });
        }
        let held = current.and_then(|v| v.get(probe_key)).is_none();
        self.probes.insert(cache_key, held);
        Ok(held)
    }

    fn serialize(&self, index: usize, out: &mut String) -> Result<()> {
        // Upstream does not apply its emitted-output limit to the pre-clean
        // snapshot, including sparse-array padding outside buffered raw bytes.
        let append = |out: &mut String, text: &str| -> Result<()> {
            out.push_str(text);
            Ok(())
        };
        match &self.nodes[index].kind {
            Kind::Object(children) => {
                append(out, "{")?;
                for (at, (key, child)) in children.order.iter().enumerate() {
                    if at > 0 {
                        append(out, ",")?;
                    }
                    append(out, &bounded_quoted(key)?)?;
                    append(out, ":")?;
                    self.serialize(*child, out)?;
                }
                append(out, "}")
            }
            Kind::Array(children) => {
                append(out, "[")?;
                for at in 0..=children.last_key_value().map_or(0, |(at, _)| *at) {
                    if at > 0 {
                        append(out, ",")?;
                    }
                    if let Some(child) = children.get(&at) {
                        self.serialize(*child, out)?;
                    } else {
                        append(out, "null")?;
                    }
                }
                append(out, "]")
            }
            Kind::String { escaped, .. } => {
                append(out, "\"")?;
                append(out, escaped)?;
                append(out, "\"")
            }
            Kind::Scalar(raw) => append(out, raw),
            Kind::Unset => append(out, "null"),
        }
    }

    fn prune(&mut self, index: usize, cleaned: &Value) {
        let next = self.nodes[index].next;
        let children: Vec<_> = match &self.nodes[index].kind {
            Kind::Object(children) => children
                .order
                .iter()
                .enumerate()
                .skip(next)
                .map(|(_, (key, child))| (Segment::Key(key.clone()), *child))
                .collect(),
            Kind::Array(children) => children
                .iter()
                .map(|(at, child)| (Segment::Index(*at), *child))
                .collect(),
            _ => Vec::new(),
        };
        let mut removed = HashSet::new();
        for (segment, child) in children {
            let value = match &segment {
                Segment::Key(key) => cleaned.get(key),
                Segment::Index(at) => cleaned.get(*at),
            };
            if self.nodes[child].held && value.is_none() {
                removed.insert(child);
            } else {
                self.nodes[child].held = false;
                self.prune(child, value.unwrap_or(&Value::Null));
            }
        }
        if let Kind::Object(children) = &mut self.nodes[index].kind {
            children.order.retain(|(_, child)| !removed.contains(child));
            children.by_key.retain(|_, child| !removed.contains(child));
        }
    }

    fn emit(&mut self, index: usize, final_chunk: bool) -> Result<bool> {
        if self.nodes[index].held {
            return Ok(false);
        }
        if self.nodes[index].closed {
            return Ok(true);
        }
        match &self.nodes[index].kind {
            Kind::String { escaped, open } => {
                let open = *open;
                let from = self.nodes[index].next;
                if !self.nodes[index].opened {
                    append(&mut self.full, "\"")?;
                }
                append(&mut self.full, &escaped[from..])?;
                self.nodes[index].next = escaped.len();
                self.nodes[index].opened = true;
                if open && !final_chunk {
                    return Ok(false);
                }
                append(&mut self.full, "\"")?;
            }
            Kind::Scalar(raw) => {
                append(&mut self.full, raw)?;
            }
            Kind::Object(_) | Kind::Array(_) => {
                let object = matches!(self.nodes[index].kind, Kind::Object(_));
                if !self.nodes[index].opened {
                    if !final_chunk
                        && let Kind::Object(children) = &self.nodes[index].kind
                        && children
                            .order
                            .first()
                            .is_some_and(|(_, child)| self.nodes[*child].held)
                    {
                        return Ok(false);
                    }
                    append(&mut self.full, if object { "{" } else { "[" })?;
                    self.nodes[index].opened = true;
                }
                loop {
                    let at = self.nodes[index].next;
                    let (key, child) = match &self.nodes[index].kind {
                        Kind::Object(children) => match children.order.get(at) {
                            Some((key, child)) => (Some(key.clone()), Some(*child)),
                            None => break,
                        },
                        Kind::Array(children) => {
                            if children.last_key_value().is_none_or(|(last, _)| at > *last) {
                                break;
                            }
                            (None, children.get(&at).copied())
                        }
                        _ => unreachable!(),
                    };
                    if child.is_some_and(|child| self.nodes[child].held)
                        || (child.is_none() && !final_chunk)
                    {
                        return Ok(false);
                    }
                    if !self.nodes[index].entry_opened {
                        if at > 0 {
                            append(&mut self.full, ",")?;
                        }
                        if let Some(key) = key {
                            append(&mut self.full, &bounded_quoted(&key)?)?;
                            append(&mut self.full, ":")?;
                        }
                        self.nodes[index].entry_opened = true;
                    }
                    if let Some(child) = child {
                        if !self.emit(child, final_chunk)? {
                            return Ok(false);
                        }
                    } else {
                        append(&mut self.full, "null")?;
                    }
                    self.nodes[index].next += 1;
                    self.nodes[index].entry_opened = false;
                }
                if !final_chunk {
                    return Ok(false);
                }
                append(&mut self.full, if object { "}" } else { "]" })?;
            }
            Kind::Unset => return Err(ArgumentError("unset Vertex argument value")),
        }
        self.nodes[index].closed = true;
        Ok(true)
    }

    fn can_accept(&self, call: &Value) -> Result<bool> {
        if self.error.is_some() || self.finished {
            return Ok(false);
        }
        let Some(parts) = field(call, "partialArgs", "partial_args") else {
            return Ok(true);
        };
        let Some(parts) = parts.as_array() else {
            return Ok(false);
        };
        for part in parts {
            if !part.is_object() {
                return Ok(false);
            }
            let path = crate::json::str_of(field(part, "jsonPath", "json_path"));
            let Ok(segments) = parse_path(&path) else {
                continue;
            };
            if segments.is_empty() {
                continue;
            }
            let Some(index) = self.find(&segments)? else {
                continue;
            };
            let node = &self.nodes[index];
            if matches!(node.kind, Kind::Unset) {
                continue;
            }
            let string = field(part, "stringValue", "string_value").is_some_and(|v| !v.is_null());
            let scalar = field(part, "numberValue", "number_value").is_some_and(|v| !v.is_null())
                || field(part, "boolValue", "bool_value").is_some_and(|v| !v.is_null())
                || field(part, "nullValue", "null_value").is_some();
            if !string && !scalar && !will_continue(part) {
                continue;
            }
            if !(string || will_continue(part))
                || node.closed
                || !matches!(node.kind, Kind::String { open: true, .. })
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub(crate) struct StreamingCall {
    pub(crate) raw_name: String,
    pub(crate) upstream_id: String,
    pub(crate) id: String,
    pub(crate) index: i64,
    pub(crate) assembler: Assembler,
}

impl StreamingCall {
    pub(crate) fn new(raw_name: String, upstream_id: String, id: String, index: i64) -> Self {
        Self {
            raw_name,
            upstream_id,
            id,
            index,
            assembler: Assembler::default(),
        }
    }

    pub(crate) fn can_continue(&self, call: &Value) -> bool {
        if !call.is_object() || (call.get("args").is_some() && !is_streaming(call)) {
            return false;
        }
        let id = crate::json::str_of(call.get("id"));
        let id = id.trim();
        if !id.is_empty() && !self.upstream_id.is_empty() && id != self.upstream_id {
            return false;
        }
        let name = crate::json::str_of(call.get("name"));
        let name = name.trim();
        name.is_empty()
            || (name == self.raw_name
                && ((!id.is_empty() && id == self.upstream_id)
                    || self.assembler.can_accept(call).unwrap_or(true)))
    }
}

pub(crate) fn will_continue(call: &Value) -> bool {
    call.get("willContinue").is_some_and(crate::json::bool_of)
        || call.get("will_continue").is_some_and(crate::json::bool_of)
}

pub(crate) fn is_streaming(call: &Value) -> bool {
    if !call.is_object() {
        return false;
    }
    if let Some(parts) = field(call, "partialArgs", "partial_args") {
        return !(parts.as_array().is_some_and(Vec::is_empty)
            && call.get("args").is_some()
            && !will_continue(call));
    }
    (call.get("willContinue").is_some() || call.get("will_continue").is_some())
        && (call.get("args").is_none() || will_continue(call))
}

fn field<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    value.get(camel).or_else(|| value.get(snake))
}

fn container(segment: &Segment) -> Kind {
    match segment {
        Segment::Key(_) => Kind::Object(ObjectChildren::default()),
        Segment::Index(_) => Kind::Array(BTreeMap::new()),
    }
}

fn number_text(value: &Value) -> Result<String> {
    if value.is_number() {
        return bounded_json(value);
    }
    if let Some(text) = value.as_str()
        && let Ok(number) = serde_json::from_str::<Value>(text.trim())
        && number.is_number()
    {
        return Ok(text.trim().to_owned());
    }
    let number =
        crate::json::float_of(value).ok_or(ArgumentError("non-finite Vertex argument number"))?;
    bounded_json(&number)
}

fn append(out: &mut String, text: &str) -> Result<()> {
    if text.len() > MAX_BYTES.saturating_sub(out.len()) {
        return Err(ArgumentError("Vertex argument size limit exceeded"));
    }
    out.push_str(text);
    Ok(())
}

struct BoundedWriter(Vec<u8>, usize);

impl io::Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.1.saturating_sub(self.0.len()) {
            return Err(io::Error::other("Vertex argument size limit exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounded_quoted(value: &str) -> Result<String> {
    let mut writer = BoundedWriter(Vec::new(), MAX_QUOTED_BYTES);
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| ArgumentError("Vertex argument size limit exceeded"))?;
    String::from_utf8(writer.0).map_err(|_| ArgumentError("invalid Vertex argument UTF-8"))
}

fn bounded_json(value: &Value) -> Result<String> {
    let mut writer = BoundedWriter(Vec::new(), MAX_BYTES);
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| ArgumentError("Vertex argument size limit exceeded"))?;
    String::from_utf8(writer.0).map_err(|_| ArgumentError("invalid Vertex argument UTF-8"))
}

fn parse_path(raw: &str) -> Result<Vec<Segment>> {
    let invalid = ArgumentError("invalid Vertex argument JSON path");
    let path = raw.trim();
    if path.is_empty() || path == "$" {
        return Ok(Vec::new());
    }
    let mut rest = path.strip_prefix('$').ok_or(invalid)?;
    let mut segments = Vec::new();
    while !rest.is_empty() {
        if segments.len() == MAX_DEPTH {
            return Err(invalid);
        }
        if let Some(dot) = rest.strip_prefix('.') {
            let end = dot.find(['.', '[']).unwrap_or(dot.len());
            let key = &dot[..end];
            if key.is_empty() || key.bytes().any(|c| c < 0x20) {
                return Err(invalid);
            }
            segments.push(Segment::Key(key.to_owned()));
            rest = &dot[end..];
        } else if let Some(bracket) = rest.strip_prefix('[') {
            if bracket.starts_with(['\'', '"']) {
                let quote = bracket.as_bytes()[0];
                let mut key = String::from("\"");
                let mut at = 1;
                let bytes = bracket.as_bytes();
                loop {
                    let c = *bytes.get(at).ok_or(invalid)?;
                    at += 1;
                    if c == quote {
                        break;
                    }
                    if c < 0x20 {
                        return Err(invalid);
                    }
                    if c == b'\\' {
                        let escape = *bytes.get(at).ok_or(invalid)?;
                        if !escape.is_ascii() {
                            return Err(invalid);
                        }
                        at += 1;
                        if escape == b'\'' {
                            key.push('\'');
                        } else {
                            key.push('\\');
                            key.push(char::from(escape));
                        }
                    } else if c == b'"' {
                        key.push_str("\\\"");
                    } else {
                        // Copy complete UTF-8 characters, not individual bytes.
                        let start = at - 1;
                        let character = bracket[start..].chars().next().ok_or(invalid)?;
                        key.push(character);
                        at = start + character.len_utf8();
                    }
                }
                if bytes.get(at) != Some(&b']') {
                    return Err(invalid);
                }
                key.push('"');
                let key = serde_json::from_str::<String>(&key).map_err(|_| invalid)?;
                segments.push(Segment::Key(key));
                rest = &bracket[at + 1..];
            } else {
                let end = bracket.find(']').ok_or(invalid)?;
                let digits = &bracket[..end];
                if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
                    return Err(invalid);
                }
                let index = digits.parse::<usize>().map_err(|_| invalid)?;
                if index > MAX_INDEX {
                    return Err(invalid);
                }
                segments.push(Segment::Index(index));
                rest = &bracket[end + 1..];
            }
        } else {
            return Err(invalid);
        }
    }
    Ok(segments)
}

#[cfg(test)]
mod tests;
