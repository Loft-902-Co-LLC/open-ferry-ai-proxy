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
const MAX_NODES: usize = 65_536;
const MAX_WORK: usize = 64 * 1024 * 1024;

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
    Object(Vec<(String, usize)>),
    Array(BTreeMap<usize, usize>),
    String {
        escaped: String,
        open: bool,
    },
    Scalar(String),
}

#[derive(Default)]
struct Node {
    kind: Kind,
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
    work: usize,
    full: String,
    error: Option<ArgumentError>,
    finished: bool,
}

impl Assembler {
    pub(crate) fn push(&mut self, call: &Value, cleaner: Cleaner<'_>) -> Result<(String, bool)> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let result = self.push_inner(call, cleaner);
        if let Err(error) = result {
            self.error = Some(error);
            // Never expose a partially valid final argument object after failure.
            self.full.clear();
        }
        result
    }

    fn push_inner(&mut self, call: &Value, cleaner: Cleaner<'_>) -> Result<(String, bool)> {
        if self.finished {
            return Err(ArgumentError("Vertex arguments received after completion"));
        }
        // Bound malformed typed values too, before coercion or cleaner copies.
        self.charge_work(bounded_json(call)?.len())?;
        let start = self.full.len();
        let final_chunk = !will_continue(call);
        if let Some(parts) = field(call, "partialArgs", "partial_args") {
            let parts = parts
                .as_array()
                .ok_or(ArgumentError("Vertex partialArgs must be an array"))?;
            for part in parts {
                self.push_part(part, cleaner)?;
            }
        }
        if final_chunk && self.root.is_none() {
            let empty = serde_json::json!({});
            let args = call
                .get("args")
                .filter(|args| args.is_object())
                .unwrap_or(&empty);
            // Check the borrowed input before the cleaner can copy it.
            let raw = bounded_json(args)?;
            self.charge_work(raw.len())?;
            self.full = match cleaner {
                Some(clean) => bounded_json(&clean(args))?,
                None => raw,
            };
        } else if let Some(root) = self.root {
            if final_chunk && let Some(clean) = cleaner {
                let mut raw = String::new();
                self.serialize(root, &mut raw)?;
                self.charge_work(raw.len())?;
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

    fn charge_work(&mut self, bytes: usize) -> Result<()> {
        self.work = self.work.saturating_add(bytes);
        if self.work > MAX_WORK {
            return Err(ArgumentError("Vertex argument work limit exceeded"));
        }
        Ok(())
    }

    fn reserve(&mut self, bytes: usize) -> Result<()> {
        self.buffered = self.buffered.saturating_add(bytes);
        if self.buffered > MAX_BYTES {
            return Err(ArgumentError("Vertex argument size limit exceeded"));
        }
        Ok(())
    }

    fn alloc(&mut self, kind: Kind, held: bool) -> Result<usize> {
        if self.nodes.len() >= MAX_NODES {
            return Err(ArgumentError("Vertex argument node limit exceeded"));
        }
        self.reserve(4)?;
        let index = self.nodes.len();
        self.nodes.push(Node {
            kind,
            held,
            ..Node::default()
        });
        Ok(index)
    }

    fn find(&self, segments: &[Segment]) -> Option<usize> {
        let mut current = self.root?;
        for segment in segments {
            current = match (&self.nodes[current].kind, segment) {
                (Kind::Object(children), Segment::Key(key)) => children
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, child)| *child)?,
                (Kind::Array(children), Segment::Index(index)) => *children.get(index)?,
                _ => return None,
            };
        }
        Some(current)
    }

    fn push_part(&mut self, part: &Value, cleaner: Cleaner<'_>) -> Result<()> {
        if !part.is_object() {
            return Err(ArgumentError(
                "Vertex partialArgs element must be an object",
            ));
        }
        let path = field(part, "jsonPath", "json_path");
        let raw_path = path.and_then(Value::as_str).unwrap_or_default();
        self.charge_work(raw_path.len())?;
        let segments = parse_path(raw_path)?;
        let string = field(part, "stringValue", "string_value").filter(|v| !v.is_null());
        let number = field(part, "numberValue", "number_value").filter(|v| !v.is_null());
        let boolean = field(part, "boolValue", "bool_value").filter(|v| !v.is_null());
        let null = field(part, "nullValue", "null_value").is_some();
        let continuing = will_continue(part);
        if string.is_none() && number.is_none() && boolean.is_none() && !null && !continuing {
            let node = if segments.is_empty() {
                self.last_string
            } else {
                self.find(&segments)
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
                (Kind::Object(children), Segment::Key(key)) => {
                    self.charge_lookup(children.len())?;
                    match &self.nodes[current].kind {
                        Kind::Object(children) => children
                            .iter()
                            .find(|(name, _)| name == key)
                            .map(|(_, node)| *node),
                        _ => unreachable!(),
                    }
                }
                (Kind::Array(children), Segment::Index(index)) => children.get(index).copied(),
                _ => return Err(ArgumentError("conflicting Vertex container types")),
            };
            let child = match found {
                Some(child) => child,
                None => {
                    let held = match segment {
                        Segment::Key(key) => {
                            self.reserve(key.len())?;
                            self.should_hold(&segments[..at], key, cleaner)?
                        }
                        Segment::Index(_) => false,
                    };
                    let kind = segments.get(at + 1).map_or(Kind::Unset, container);
                    let child = self.alloc(kind, held)?;
                    let next = self.nodes[current].next;
                    let insert = match &self.nodes[current].kind {
                        Kind::Object(children) if !held => children
                            .iter()
                            .enumerate()
                            .skip(next)
                            .find(|(_, (_, id))| self.nodes[*id].held)
                            .map_or(children.len(), |(at, _)| at),
                        Kind::Object(children) => children.len(),
                        _ => 0,
                    };
                    match (&mut self.nodes[current].kind, segment) {
                        (Kind::Object(children), Segment::Key(key)) => {
                            children.insert(insert, (key.clone(), child));
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
            let quoted = bounded_quoted(&value)?;
            let escaped = &quoted[1..quoted.len() - 1];
            self.reserve(escaped.len())?;
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
                number_text(number)?
            } else if let Some(boolean) = boolean {
                crate::json::bool_of(boolean).to_string()
            } else {
                "null".to_owned()
            };
            self.reserve(scalar.len())?;
            self.nodes[current].kind = Kind::Scalar(scalar);
        }
        Ok(())
    }

    fn charge_lookup(&mut self, count: usize) -> Result<()> {
        self.charge_work(count)
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
        let key_bytes = path
            .iter()
            .map(|segment| match segment {
                Segment::Key(key) => key.len(),
                Segment::Index(_) => std::mem::size_of::<usize>(),
            })
            .sum::<usize>()
            .saturating_add(probe_key.len());
        self.charge_work(key_bytes)?;
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
        self.reserve(key_bytes)?;
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
        self.charge_work(bounded_json(&probe)?.len())?;
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
        match &self.nodes[index].kind {
            Kind::Object(children) => {
                append(out, "{")?;
                for (at, (key, child)) in children.iter().enumerate() {
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
            children.retain(|(_, child)| !removed.contains(child));
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
                        Kind::Object(children) => match children.get(at) {
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

    fn can_accept(&self, call: &Value) -> bool {
        if self.error.is_some() || self.finished {
            return false;
        }
        let Some(parts) = field(call, "partialArgs", "partial_args") else {
            return true;
        };
        let Some(parts) = parts.as_array() else {
            return false;
        };
        parts.iter().all(|part| {
            if !part.is_object() {
                return false;
            }
            let Some(path) = field(part, "jsonPath", "json_path").and_then(Value::as_str) else {
                return true;
            };
            let Ok(segments) = parse_path(path) else {
                return true;
            };
            let Some(index) = self.find(&segments) else {
                return true;
            };
            let node = &self.nodes[index];
            if matches!(node.kind, Kind::Unset) {
                return true;
            }
            let string = field(part, "stringValue", "string_value").is_some_and(|v| !v.is_null());
            let scalar = field(part, "numberValue", "number_value").is_some_and(|v| !v.is_null())
                || field(part, "boolValue", "bool_value").is_some_and(|v| !v.is_null())
                || field(part, "nullValue", "null_value").is_some();
            if !string && !scalar && !will_continue(part) {
                return true;
            }
            (string || will_continue(part))
                && !node.closed
                && matches!(node.kind, Kind::String { open: true, .. })
        })
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
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if !id.is_empty() && !self.upstream_id.is_empty() && id != self.upstream_id {
            return false;
        }
        let name = call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        name.is_empty()
            || (name == self.raw_name
                && ((!id.is_empty() && id == self.upstream_id) || self.assembler.can_accept(call)))
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
        Segment::Key(_) => Kind::Object(Vec::new()),
        Segment::Index(_) => Kind::Array(BTreeMap::new()),
    }
}

fn number_text(value: &Value) -> Result<String> {
    if value.is_number() {
        return bounded_json(value);
    }
    if value.as_str().is_some_and(|text| text.len() > MAX_BYTES) {
        return Err(ArgumentError("Vertex argument size limit exceeded"));
    }
    if let Some(text) = value.as_str()
        && let Ok(number) = serde_json::from_str::<Value>(text.trim())
        && number.is_number()
    {
        return Ok(text.trim().to_owned());
    }
    Ok("0".to_owned())
}

fn append(out: &mut String, text: &str) -> Result<()> {
    if text.len() > MAX_BYTES.saturating_sub(out.len()) {
        return Err(ArgumentError("Vertex argument size limit exceeded"));
    }
    out.push_str(text);
    Ok(())
}

struct BoundedWriter(Vec<u8>);

impl io::Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_BYTES.saturating_sub(self.0.len()) {
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
    let mut writer = BoundedWriter(Vec::new());
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| ArgumentError("Vertex argument size limit exceeded"))?;
    String::from_utf8(writer.0).map_err(|_| ArgumentError("invalid Vertex argument UTF-8"))
}

fn bounded_json(value: &Value) -> Result<String> {
    fn check(value: &Value, depth: usize, nodes: &mut usize, bytes: &mut usize) -> Result<()> {
        *nodes += 1;
        if depth > MAX_DEPTH || *nodes > MAX_NODES {
            return Err(ArgumentError("Vertex argument structure limit exceeded"));
        }
        *bytes = bytes.saturating_add(match value {
            Value::String(text) => text.len(),
            _ => 1,
        });
        if *bytes > MAX_BYTES {
            return Err(ArgumentError("Vertex argument size limit exceeded"));
        }
        match value {
            Value::Array(values) => {
                for value in values {
                    check(value, depth + 1, nodes, bytes)?;
                }
            }
            Value::Object(values) => {
                for (key, value) in values {
                    *bytes = bytes.saturating_add(key.len());
                    check(value, depth + 1, nodes, bytes)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    check(value, 0, &mut 0, &mut 0)?;
    let mut writer = BoundedWriter(Vec::new());
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| ArgumentError("Vertex argument size limit exceeded"))?;
    String::from_utf8(writer.0).map_err(|_| ArgumentError("invalid Vertex argument UTF-8"))
}

fn parse_path(raw: &str) -> Result<Vec<Segment>> {
    let invalid = ArgumentError("invalid Vertex argument JSON path");
    if raw.len() > MAX_BYTES {
        return Err(invalid);
    }
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
