// Ported from CLIProxyAPI internal/config/config_v8.go (expandConfigAliases,
// yamlPath, findMapKeyIndex, setYAMLPath, deleteYAMLPath) (v8.0.20, MIT),
// with the decoding rules of gopkg.in/yaml.v3 v3.0.1 (resolve.go, decode.go,
// yaml.go) and its scalar styles (encode.go encoder.node, emitterc.go
// yaml_emitter_select_scalar_style, yaml_emitter_analyze_scalar;
// Apache-2.0), which upstream decodes and writes its config with.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/go-yaml/yaml

//! A YAML node tree that behaves like yaml.v3's.
//!
//! Upstream's config loader leans on yaml.v3 for more than syntax: which
//! plain scalars are numbers, booleans or timestamps, how duplicate keys,
//! merge keys (`<<`) and aliases are treated, and the exact wording of its
//! errors. [`parse_document`] reads the first document into a [`Node`] tree
//! with aliases expanded, [`check_shape`] repeats upstream's
//! `node.Decode(&map[string]any)` pass, [`check_decode_any`] its decode into
//! `any`, and [`expand_merges`] and the path helpers are upstream's own
//! tree edits. [`write_and_read_back`] gives a tree the tags yaml.v3 gives
//! it when it writes the tree out and reads it back, and
//! [`from_writer_node`] turns a tree of the config writer
//! ([`super::yaml3`]) into one of these, so it can be decoded as upstream
//! decodes its writer's trees. `saphyr-parser` does the scanning.
//!
//! An alias expands to a copy of its anchor's node that shares the
//! anchor's text, so the tree takes memory in proportion to the input
//! however often an alias repeats a long scalar. Text that aliases produce
//! when the tree is decoded into owned values is counted by an
//! [`AliasBudget`].
//!
//! [`Node`]'s `Debug` shows a node's kind, line and size, never its text,
//! which may be a secret.
//!
//! Deviations from upstream:
//! - Syntax error wording is saphyr's, as `yaml: line N: <message>`; only the
//!   line matches yaml.v3. saphyr also accepts a few inputs yaml.v3 rejects,
//!   such as a tab before a top-level key, and rejects a few it accepts,
//!   such as a tab right after a key's `:`.
//! - Nesting deeper than 256 levels, aliases included, is an error
//!   (`exceeded max depth of 256`); yaml.v3 allows 10000.
//! - Type errors (`cannot unmarshal`, `cannot decode`) leave out yaml.v3's
//!   excerpt of the offending value, which may be a secret, and at most 100
//!   are reported.
//! - `invalid map key` errors don't print the key.
//! - An alias cycle or excessive aliasing under a key that the decode pass
//!   skips (a null key) is still an error, and keys read after excessive
//!   aliasing is detected aren't checked for duplicates.
//! - Flow collections nest at most 255 deep (saphyr's limit).
//! - `strconv.Quote`'s notion of a printable rune is approximated: control
//!   characters, white space other than U+0020 and the common format
//!   characters are escaped.
//! - `!!binary` values that don't decode to UTF-8 are converted lossily.
//! - A character yaml.v3's reader refuses (`control characters are not
//!   allowed`) is an error anywhere in the input. yaml.v3 reads ahead in
//!   chunks and only refuses what it reads, so one deep in a second
//!   document can go unnoticed there.
//! - Decoding a tree into owned values (the config, or a v8 read's value)
//!   stops with `document contains excessive aliasing` once the scalars
//!   that aliases expanded to add up to more than 64 MiB. yaml.v3 shares a
//!   Go string between its copies and has no such limit, beyond its ratio
//!   of aliases to nodes, which applies here too.
//! - An alias used as a mapping key with the `:` right after its name
//!   (`*k: v`) is read as yaml.v3 reads it only when the `:` is followed by
//!   a space, a line break or the end of the input. saphyr takes the `:` as
//!   part of the name, so the others (`{*k:1}`, `[*k:]`) are an `unknown
//!   anchor` error.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt::{self, Write as _};
use std::ops::Deref;
use std::sync::Arc;

use saphyr_parser::{Event, Parser, ScalarStyle, ScanError, Span, Tag};

use super::yaml3;

/// The deepest nesting accepted, counting expanded aliases.
pub(crate) const MAX_DEPTH: usize = 256;

/// The most type errors one decode reports.
const MAX_TYPE_ERRORS: usize = 100;

/// The most bytes of scalar text that aliases may produce in one decode
/// into owned values.
pub(crate) const MAX_ALIASED_BYTES: usize = 64 << 20;

const EXCESSIVE_ALIASING: &str = "document contains excessive aliasing";
const WANT_MAP: &str = "map merge requires map or sequence of maps as the value";

/// A node's tag or value. Clones share the text, so the copies an alias
/// expands to cost no more than the text once. Its `Debug` hides it.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub(crate) struct Text(Arc<str>);

impl Text {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `self` and `other` share their text.
    #[cfg(test)]
    pub(crate) fn shares(&self, other: &Text) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Deref for Text {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Self(Arc::from(text))
    }
}

impl From<String> for Text {
    fn from(text: String) -> Self {
        Self(Arc::from(text))
    }
}

impl PartialEq<str> for Text {
    fn eq(&self, other: &str) -> bool {
        *self.0 == *other
    }
}

impl PartialEq<&str> for Text {
    fn eq(&self, other: &&str) -> bool {
        *self.0 == **other
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("\"\"")
        } else {
            f.write_str("<redacted>")
        }
    }
}

/// The kind of a [`Node`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    #[default]
    Scalar,
    Sequence,
    Mapping,
    /// A node yaml.v3 refuses to decode; its value is the error message.
    Poison,
}

/// Where an alias stood, kept on the node it expanded to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AliasRef {
    pub(crate) name: Text,
    pub(crate) line: usize,
}

/// A YAML node, like yaml.v3's `Node` with aliases expanded.
#[derive(Clone, Default, PartialEq)]
pub(crate) struct Node {
    pub(crate) kind: Kind,
    /// The short tag, such as `!!str`, `!!int`, `!!map` or `!custom`.
    pub(crate) tag: Text,
    pub(crate) value: Text,
    /// Children: sequence items, or mapping keys and values in turn.
    pub(crate) content: Vec<Node>,
    /// The 1-based source line; 0 for nodes built in code.
    pub(crate) line: usize,
    /// Set when this node is an alias's expansion.
    pub(crate) alias: Option<Box<AliasRef>>,
    /// Set on a collection written in flow style (yaml.v3's `FlowStyle`).
    pub(crate) flow: bool,
    /// Set on a scalar whose tag was written out (yaml.v3's `TaggedStyle`).
    pub(crate) tagged: bool,
    /// Set on an alias's expansion and everything in it.
    pub(crate) aliased: bool,
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("kind", &self.kind)
            .field("line", &self.line)
            .field("content", &self.content.len())
            .finish_non_exhaustive()
    }
}

impl Node {
    /// A scalar node.
    pub(crate) fn scalar(tag: &str, value: &str) -> Self {
        Self {
            kind: Kind::Scalar,
            tag: tag.into(),
            value: value.into(),
            ..Self::default()
        }
    }

    /// An empty mapping node.
    pub(crate) fn mapping() -> Self {
        Self {
            kind: Kind::Mapping,
            tag: "!!map".into(),
            ..Self::default()
        }
    }

    /// An empty sequence node.
    pub(crate) fn sequence() -> Self {
        Self {
            kind: Kind::Sequence,
            tag: "!!seq".into(),
            ..Self::default()
        }
    }

    pub(crate) fn is_mapping(&self) -> bool {
        self.kind == Kind::Mapping
    }

    /// Mapping keys and values as pairs.
    pub(crate) fn pairs(&self) -> impl Iterator<Item = (&Node, &Node)> {
        self.content
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[key, value]| (key, value))
    }

    /// yaml.v3's `ShortTag`, where an alias has no tag.
    fn go_short_tag(&self) -> &str {
        if self.alias.is_some() {
            ""
        } else {
            self.tag.as_str()
        }
    }

    /// The node's kind and value as yaml.v3 compares keys for duplicates.
    fn go_identity(&self) -> (u8, &str) {
        if let Some(alias) = &self.alias {
            return (4, alias.name.as_str());
        }
        match self.kind {
            Kind::Scalar => (1, self.value.as_str()),
            Kind::Sequence => (2, ""),
            Kind::Mapping => (3, ""),
            Kind::Poison => (5, ""),
        }
    }

    fn go_line(&self) -> usize {
        self.alias.as_ref().map_or(self.line, |alias| alias.line)
    }
}

/// An error from reading or decoding YAML, worded as yaml.v3 words it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum YamlError {
    /// The text isn't YAML. The message is complete.
    Syntax(String),
    /// A decode stopped (yaml.v3's `failf`). The message lacks `yaml: `.
    Fatal(String),
    /// Values of the wrong type (yaml.v3's `TypeError`).
    Type(Vec<String>),
}

impl YamlError {
    /// The message yaml.v3 would return.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Syntax(message) => message.clone(),
            Self::Fatal(message) => format!("yaml: {message}"),
            Self::Type(errors) => format!("yaml: unmarshal errors:\n  {}", errors.join("\n  ")),
        }
    }
}

/// Adds a type error, up to the cap.
pub(crate) fn push_type_error(errors: &mut Vec<String>, error: String) {
    if errors.len() < MAX_TYPE_ERRORS {
        errors.push(error);
    }
}

/// yaml.v3's type error for a node that can't go into `type_name`. An
/// alias reports its target, as yaml.v3 does.
pub(crate) fn type_error(node: &Node, type_name: &str) -> String {
    format!(
        "line {}: cannot unmarshal {} into {type_name}",
        node.line, node.tag
    )
}

/// Counts the scalar text that aliases produce in one decode into owned
/// values. The tree shares an anchor's text with its aliases; owned values
/// can't, so without a limit a short document could decode to gigabytes.
#[derive(Default)]
pub(crate) struct AliasBudget(Cell<usize>);

impl AliasBudget {
    /// Counts `node`'s text when an alias produced it. An error once more
    /// than [`MAX_ALIASED_BYTES`] has been counted.
    pub(crate) fn charge(&self, node: &Node) -> Result<(), YamlError> {
        if !node.aliased {
            return Ok(());
        }
        let total = self.0.get().saturating_add(node.value.len());
        self.0.set(total);
        if total > MAX_ALIASED_BYTES {
            return Err(YamlError::Fatal(EXCESSIVE_ALIASING.to_owned()));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reading

enum Raw {
    Scalar {
        tag: Text,
        tagged: bool,
        value: Text,
        line: usize,
    },
    Collection {
        kind: Kind,
        tag: Text,
        flow: bool,
        items: Vec<usize>,
        line: usize,
    },
    Alias {
        target: usize,
        name: Text,
        line: usize,
    },
    /// An alias inside its own anchor; the message says so.
    Cyclic { message: Text, line: usize },
}

impl Raw {
    fn line(&self) -> usize {
        match self {
            Self::Scalar { line, .. }
            | Self::Collection { line, .. }
            | Self::Alias { line, .. }
            | Self::Cyclic { line, .. } => *line,
        }
    }
}

/// Reads anchor names from the source by character index.
struct NameReader<'s> {
    source: &'s str,
    chars: usize,
    bytes: usize,
}

impl<'s> NameReader<'s> {
    fn new(source: &'s str) -> Self {
        Self {
            source,
            chars: 0,
            bytes: 0,
        }
    }

    /// The anchor name of the alias at character `index` (its `*`).
    fn alias_name(&mut self, index: usize) -> String {
        if index < self.chars {
            self.chars = 0;
            self.bytes = 0;
        }
        let rest = self.source.get(self.bytes..).unwrap_or("");
        let mut offset = rest.len();
        for (count, (byte, _)) in rest.char_indices().enumerate() {
            if self.chars + count == index {
                offset = byte;
                break;
            }
        }
        self.chars = index;
        self.bytes += offset;
        let text = self.source.get(self.bytes..).unwrap_or("");
        let text = text.strip_prefix('*').unwrap_or(text);
        text.chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, ',' | '[' | ']' | '{' | '}'))
            .collect()
    }
}

fn full_tag(tag: &Tag) -> String {
    format!("{}{}", tag.handle, tag.suffix)
}

/// yaml.v3's `shortTag`.
fn short_tag(tag: &str) -> String {
    match tag.strip_prefix("tag:yaml.org,2002:") {
        Some(rest) => format!("!!{rest}"),
        None => tag.to_owned(),
    }
}

/// The tag yaml.v3's parser gives a node (`parser.node`), and whether it
/// was written out (`TaggedStyle`).
fn node_tag(explicit: Option<&Tag>, kind: Kind, style: ScalarStyle, value: &str) -> (Text, bool) {
    if let Some(tag) = explicit {
        let full = full_tag(tag);
        if !full.is_empty() && full != "!" {
            return (short_tag(&full).into(), true);
        }
    }
    let tag = match kind {
        Kind::Mapping => "!!map",
        Kind::Sequence => "!!seq",
        _ if style != ScalarStyle::Plain => "!!str",
        _ if value == "<<" => "!!merge",
        _ => resolve_plain("", value).0,
    };
    (tag.into(), false)
}

fn depth_error(line: usize) -> YamlError {
    YamlError::Syntax(format!(
        "yaml: line {line}: exceeded max depth of {MAX_DEPTH}"
    ))
}

fn scan_error(names: &mut NameReader<'_>, error: &ScanError) -> YamlError {
    let marker = error.marker();
    if error.info().contains("unknown anchor") {
        let name = names.alias_name(marker.index());
        return YamlError::Syntax(format!("yaml: unknown anchor '{name}' referenced"));
    }
    YamlError::Syntax(format!("yaml: line {}: {}", marker.line(), error.info()))
}

/// Builds the node arena from parser events.
struct Builder<'s> {
    raws: Vec<Raw>,
    anchors: HashMap<usize, usize>,
    open: Vec<usize>,
    root: Option<usize>,
    names: NameReader<'s>,
}

impl<'s> Builder<'s> {
    fn new(source: &'s str) -> Self {
        Self {
            raws: Vec::new(),
            anchors: HashMap::new(),
            open: Vec::new(),
            root: None,
            names: NameReader::new(source),
        }
    }

    fn add(&mut self, raw: Raw, anchor: usize) -> Result<(), YamlError> {
        if self.open.len() >= MAX_DEPTH {
            return Err(depth_error(raw.line()));
        }
        let index = self.raws.len();
        self.raws.push(raw);
        if anchor > 0 {
            self.anchors.insert(anchor, index);
        }
        match self.open.last().copied() {
            Some(parent) => {
                if let Some(Raw::Collection { items, .. }) = self.raws.get_mut(parent) {
                    items.push(index);
                }
            }
            None => {
                if self.root.is_none() {
                    self.root = Some(index);
                }
            }
        }
        Ok(())
    }

    /// Opens a collection. It is in flow style when its start token takes
    /// up text (`[` or `{`; saphyr gives block collections and a flow
    /// sequence's single-pair mappings empty spans) or its parent is.
    fn open(
        &mut self,
        kind: Kind,
        anchor: usize,
        tag: Option<&Tag>,
        span: Span,
    ) -> Result<(), YamlError> {
        let index = self.raws.len();
        let (tag, _) = node_tag(tag, kind, ScalarStyle::Plain, "");
        let in_flow = self.open.last().is_some_and(|&parent| {
            matches!(
                self.raws.get(parent),
                Some(Raw::Collection { flow: true, .. })
            )
        });
        let flow = in_flow || span.end.index() > span.start.index();
        self.add(
            Raw::Collection {
                kind,
                tag,
                flow,
                items: Vec::new(),
                line: span.start.line(),
            },
            anchor,
        )?;
        self.open.push(index);
        Ok(())
    }

    /// Feeds one event; returns true at the end of the first document.
    fn event(&mut self, event: Event<'_>, span: Span) -> Result<bool, YamlError> {
        let line = span.start.line();
        match event {
            Event::DocumentEnd | Event::StreamEnd => return Ok(true),
            Event::Nothing | Event::StreamStart | Event::DocumentStart(_) => {}
            Event::Scalar(value, style, anchor, tag) => {
                let (tag, tagged) = node_tag(tag.as_deref(), Kind::Scalar, style, &value);
                let raw = Raw::Scalar {
                    tag,
                    tagged,
                    value: Text::from(value.as_ref()),
                    line,
                };
                self.add(raw, anchor)?;
            }
            Event::SequenceStart(anchor, tag) => {
                self.open(Kind::Sequence, anchor, tag.as_deref(), span)?
            }
            Event::MappingStart(anchor, tag) => {
                self.open(Kind::Mapping, anchor, tag.as_deref(), span)?
            }
            Event::SequenceEnd | Event::MappingEnd => {
                self.open.pop();
            }
            Event::Alias(anchor) => {
                let name = self.names.alias_name(span.start.index());
                let Some(&target) = self.anchors.get(&anchor) else {
                    return Err(YamlError::Syntax(format!(
                        "yaml: unknown anchor '{name}' referenced"
                    )));
                };
                let raw = if self.open.contains(&target) {
                    let message = format!("anchor '{name}' value contains itself");
                    Raw::Cyclic {
                        message: message.into(),
                        line,
                    }
                } else {
                    Raw::Alias {
                        target,
                        name: name.into(),
                        line,
                    }
                };
                self.add(raw, 0)?;
            }
        }
        Ok(false)
    }
}

/// Expands the arena into a tree, counting decodes as yaml.v3 does to stop
/// alias bombs.
struct Expander<'a> {
    raws: &'a [Raw],
    decodes: u64,
    aliases: u64,
    alias_depth: u32,
    poisoned: bool,
}

fn allowed_alias_ratio(decodes: u64) -> f64 {
    const LOW: u64 = 400_000;
    const HIGH: u64 = 4_000_000;
    if decodes <= LOW {
        0.99
    } else if decodes >= HIGH {
        0.10
    } else {
        0.99 - 0.89 * ((decodes - LOW) as f64 / (HIGH - LOW) as f64)
    }
}

/// Whether yaml.v3's decoder stops with `document contains excessive
/// aliasing` after `decodes` nodes, `aliases` of them reached through an
/// alias.
fn excessive_aliasing(decodes: u64, aliases: u64) -> bool {
    aliases > 100
        && decodes > 1000
        && aliases as f64 / decodes as f64 > allowed_alias_ratio(decodes)
}

fn poison(message: Text, line: usize) -> Node {
    Node {
        kind: Kind::Poison,
        value: message,
        line,
        ..Node::default()
    }
}

impl Expander<'_> {
    fn node(&mut self, index: usize, depth: usize) -> Result<Node, YamlError> {
        let Some(raw) = self.raws.get(index) else {
            return Err(YamlError::Fatal("internal error: missing node".to_owned()));
        };
        if depth > MAX_DEPTH {
            return Err(depth_error(raw.line()));
        }
        if self.poisoned {
            return Ok(poison(EXCESSIVE_ALIASING.into(), raw.line()));
        }
        self.decodes += 1;
        let aliased = self.alias_depth > 0;
        if aliased {
            self.aliases += 1;
        }
        if excessive_aliasing(self.decodes, self.aliases) {
            self.poisoned = true;
            return Ok(poison(EXCESSIVE_ALIASING.into(), raw.line()));
        }
        match raw {
            Raw::Scalar {
                tag,
                tagged,
                value,
                line,
            } => Ok(Node {
                kind: Kind::Scalar,
                tag: tag.clone(),
                value: value.clone(),
                line: *line,
                tagged: *tagged,
                aliased,
                ..Node::default()
            }),
            Raw::Collection {
                kind,
                tag,
                flow,
                items,
                line,
            } => {
                let mut content = Vec::with_capacity(items.len());
                for &item in items {
                    content.push(self.node(item, depth + 1)?);
                }
                Ok(Node {
                    kind: *kind,
                    tag: tag.clone(),
                    content,
                    line: *line,
                    flow: *flow,
                    aliased,
                    ..Node::default()
                })
            }
            Raw::Alias { target, name, line } => {
                self.alias_depth += 1;
                let expanded = self.node(*target, depth);
                self.alias_depth -= 1;
                let mut node = expanded?;
                if node.kind != Kind::Poison {
                    node.alias = Some(Box::new(AliasRef {
                        name: name.clone(),
                        line: *line,
                    }));
                }
                Ok(node)
            }
            Raw::Cyclic { message, line } => Ok(poison(message.clone(), *line)),
        }
    }
}

/// A tree of the config writer ([`super::yaml3`]) as this module's tree,
/// as yaml.v3's decoder walks it: aliases expanded and counted as the
/// decoder counts them, and an alias cycle, an unknown anchor or a node of
/// no kind kept as the error the decoder gives for it. Decoding the result
/// fails where upstream's `node.Decode` of the writer's tree fails, with
/// its message.
pub(crate) fn from_writer_node(node: &yaml3::Node) -> Result<Node, YamlError> {
    let mut converter = Converter {
        decodes: 0,
        aliases: 0,
        alias_depth: 0,
        poisoned: false,
    };
    converter.node(node, 1)
}

/// [`from_writer_node`]'s walk, counting decodes as [`Expander`] does.
struct Converter {
    decodes: u64,
    aliases: u64,
    alias_depth: u32,
    poisoned: bool,
}

impl Converter {
    fn node(&mut self, node: &yaml3::Node, depth: usize) -> Result<Node, YamlError> {
        use yaml3::{AliasTarget, Kind as WriterKind, Style};

        if node.kind == WriterKind::Document {
            return match node.content.first() {
                Some(root) => self.node(root, depth),
                None => Ok(Node::scalar("!!null", "")),
            };
        }
        if depth > MAX_DEPTH {
            return Err(depth_error(node.line));
        }
        if self.poisoned {
            return Ok(poison(EXCESSIVE_ALIASING.into(), node.line));
        }
        self.decodes += 1;
        let aliased = self.alias_depth > 0;
        if aliased {
            self.aliases += 1;
        }
        if excessive_aliasing(self.decodes, self.aliases) {
            self.poisoned = true;
            return Ok(poison(EXCESSIVE_ALIASING.into(), node.line));
        }
        let kind = match node.kind {
            WriterKind::Scalar => {
                return Ok(Node {
                    kind: Kind::Scalar,
                    tag: node.short_tag().into(),
                    value: node.value.as_str().into(),
                    line: node.line,
                    tagged: node.style.has(Style::TAGGED),
                    aliased,
                    ..Node::default()
                });
            }
            WriterKind::Sequence => Kind::Sequence,
            WriterKind::Mapping => Kind::Mapping,
            WriterKind::Alias => {
                let target = match &node.alias {
                    Some(AliasTarget::Node(target)) => target,
                    Some(AliasTarget::Cycle) => {
                        let message = format!("anchor '{}' value contains itself", node.value);
                        return Ok(poison(message.into(), node.line));
                    }
                    None => {
                        let message = format!("unknown anchor '{}' referenced", node.value);
                        return Ok(poison(message.into(), node.line));
                    }
                };
                self.alias_depth += 1;
                let expanded = self.node(target, depth);
                self.alias_depth -= 1;
                let mut expanded = expanded?;
                if expanded.kind != Kind::Poison {
                    expanded.alias = Some(Box::new(AliasRef {
                        name: node.value.as_str().into(),
                        line: node.line,
                    }));
                }
                return Ok(expanded);
            }
            WriterKind::Zero if node.is_zero() => {
                return Ok(Node {
                    aliased,
                    ..Node::scalar("!!null", "")
                });
            }
            WriterKind::Zero | WriterKind::Document => {
                let message = "cannot decode node with unknown kind 0";
                return Ok(poison(message.into(), node.line));
            }
        };
        let mut content = Vec::with_capacity(node.content.len());
        for child in &node.content {
            content.push(self.node(child, depth + 1)?);
        }
        Ok(Node {
            kind,
            tag: node.short_tag().into(),
            content,
            line: node.line,
            flow: node.style.has(Style::FLOW),
            aliased,
            ..Node::default()
        })
    }
}

/// Reads the first YAML document; `None` when there is none.
pub(crate) fn parse_document(text: &str) -> Result<Option<Node>, YamlError> {
    let text = text
        .strip_prefix(|c: char| c as u32 == 0xFEFF)
        .unwrap_or(text);
    // saphyr would stop at a NUL and read what came before as the whole
    // document.
    if !text.chars().all(reader_accepts) {
        return Err(YamlError::Syntax(
            "yaml: control characters are not allowed".to_owned(),
        ));
    }
    let mut aliases = HashSet::new();
    let error = match read_document(text, &mut aliases) {
        Ok(root) => return Ok(root),
        Err(error) => error,
    };
    // An alias used as a key with the `:` right after its name: saphyr
    // takes the `:` as part of the name, where yaml.v3 ends the name before
    // it and reads it as the key's `:`, which may have space before it.
    // Reading again with a space before each such `:` is reading what
    // yaml.v3 reads, as long as each space follows an alias: a space after
    // anything else would change a scalar or a comment, and in a tag would
    // end it. Which of them do is learnt by splitting them all and seeing
    // where saphyr then finds aliases, and confirmed by reading again with
    // only those split.
    let keys = alias_keys(text);
    if keys.is_empty() {
        return Err(error);
    }
    aliases.clear();
    let _ = read_document(&split_alias_keys(text, &keys), &mut aliases);
    let keys: Vec<AliasKey> = keys
        .into_iter()
        .enumerate()
        .filter(|(before, key)| aliases.contains(&(key.star + before)))
        .map(|(_, key)| key)
        .collect();
    if keys.is_empty() {
        return Err(error);
    }
    aliases.clear();
    let result = read_document(&split_alias_keys(text, &keys), &mut aliases);
    let confirmed = keys
        .iter()
        .enumerate()
        .all(|(before, key)| aliases.contains(&(key.star + before)));
    if confirmed { result } else { Err(error) }
}

/// Reads the first document of `text`, noting the character index of each
/// alias saphyr reads in `aliases`, known or not.
fn read_document(text: &str, aliases: &mut HashSet<usize>) -> Result<Option<Node>, YamlError> {
    let mut builder = Builder::new(text);
    for item in Parser::new_from_str(text) {
        let (event, span) = match item {
            Ok(item) => item,
            Err(error) => {
                if error.info().contains("unknown anchor") {
                    aliases.insert(error.marker().index());
                }
                return Err(scan_error(&mut builder.names, &error));
            }
        };
        if matches!(event, Event::Alias(_)) {
            aliases.insert(span.start.index());
        }
        if builder.event(event, span)? {
            break;
        }
    }
    expand(&builder)
}

/// What may be an alias used as a key with the `:` right after its name: a
/// `*` at the start of the text or after white space, `[`, `{` or `,`, a
/// name of letters, digits, `_` and `-` (yaml.v3's anchor names), and a `:`
/// followed by a space, a line break or the end of the text.
struct AliasKey {
    /// The character index of the `*`.
    star: usize,
    /// The byte offset of the `:`.
    colon: usize,
}

/// The [`AliasKey`]s in `text`, in order.
fn alias_keys(text: &str) -> Vec<AliasKey> {
    let bytes = text.as_bytes();
    let mut keys = Vec::new();
    let mut chars = 0;
    let mut boundary = true;
    for (at, &byte) in bytes.iter().enumerate() {
        if byte == b'*' && boundary {
            let rest = bytes.get(at + 1..).unwrap_or_default();
            let name = rest
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                .count();
            let colon = at + 1 + name;
            let ends_key = matches!(bytes.get(colon + 1), None | Some(b' ' | b'\r' | b'\n'));
            if name > 0 && bytes.get(colon) == Some(&b':') && ends_key {
                keys.push(AliasKey { star: chars, colon });
            }
        }
        boundary = matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'[' | b'{' | b',');
        if byte & 0xC0 != 0x80 {
            chars += 1;
        }
    }
    keys
}

/// `text` with a space before the `:` of each of `keys`.
fn split_alias_keys(text: &str, keys: &[AliasKey]) -> String {
    let mut split = String::with_capacity(text.len() + keys.len());
    let mut from = 0;
    for key in keys {
        split.push_str(text.get(from..key.colon).unwrap_or_default());
        split.push(' ');
        from = key.colon;
    }
    split.push_str(text.get(from..).unwrap_or_default());
    split
}

/// Expands the arena [`read_document`] built into a tree.
fn expand(builder: &Builder<'_>) -> Result<Option<Node>, YamlError> {
    let Some(root) = builder.root else {
        return Ok(None);
    };
    let mut expander = Expander {
        raws: &builder.raws,
        decodes: 0,
        aliases: 0,
        alias_depth: 0,
        poisoned: false,
    };
    expander.node(root, 1).map(Some)
}

/// Whether yaml.v3's reader accepts `c` (`yaml_parser_update_buffer`).
fn reader_accepts(c: char) -> bool {
    matches!(
        u32::from(c),
        0x09 | 0x0A
            | 0x0D
            | 0x20..=0x7E
            | 0x85
            | 0xA0..=0xD7FF
            | 0xE000..=0xFFFD
            | 0x10000..=0x10_FFFF
    )
}

// ---------------------------------------------------------------------------
// Scalars

/// A resolved scalar value. Its `Debug` shows only the variant.
#[derive(Clone, PartialEq)]
pub(crate) enum Scalar {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Timestamp(Timestamp),
    Str(Text),
}

impl fmt::Debug for Scalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Null => "Null",
            Self::Bool(_) => "Bool(..)",
            Self::Int(_) => "Int(..)",
            Self::Uint(_) => "Uint(..)",
            Self::Float(_) => "Float(..)",
            Self::Timestamp(_) => "Timestamp",
            Self::Str(_) => "Str(..)",
        })
    }
}

/// A scalar's tag and value after yaml.v3's resolution.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Resolved {
    pub(crate) tag: Text,
    pub(crate) value: Scalar,
}

/// Resolves a scalar node as yaml.v3's `decoder.scalar` does, decoding
/// `!!binary` values.
pub(crate) fn resolve_node(node: &Node) -> Result<Resolved, YamlError> {
    // A string, or a tag yaml.v3 doesn't resolve: the value as it is.
    if node.tag == "!!str" || (node.tag != "!!binary" && !resolvable(&node.tag)) {
        return Ok(Resolved {
            tag: node.tag.clone(),
            value: Scalar::Str(node.value.clone()),
        });
    }
    let (tag, value) = resolve(&node.tag, &node.value).map_err(YamlError::Fatal)?;
    if tag == "!!binary" {
        let Some(bytes) = decode_base64(&node.value) else {
            return Err(YamlError::Fatal(
                "!!binary value contains invalid base64 data".to_owned(),
            ));
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        return Ok(Resolved {
            tag: tag.into(),
            value: Scalar::Str(text.into()),
        });
    }
    Ok(Resolved {
        tag: tag.into(),
        value,
    })
}

/// The text a scalar node decodes to as a Go string: its value, or the
/// decoded bytes of a `!!binary` value.
pub(crate) fn scalar_string(node: &Node) -> Result<Option<String>, YamlError> {
    let resolved = resolve_node(node)?;
    Ok(match resolved.value {
        Scalar::Null => None,
        Scalar::Str(text) if resolved.tag == "!!binary" => Some(text.to_string()),
        _ => Some(node.value.to_string()),
    })
}

fn resolvable(tag: &str) -> bool {
    matches!(
        tag,
        "" | "!!str" | "!!bool" | "!!int" | "!!float" | "!!null" | "!!timestamp"
    )
}

/// yaml.v3's `resolve`. The error is a `failf` message.
pub(crate) fn resolve(tag: &str, input: &str) -> Result<(String, Scalar), String> {
    if !resolvable(tag) {
        return Ok((tag.to_owned(), Scalar::Str(input.into())));
    }
    let (rtag, out) = resolve_plain(tag, input);
    if tag.is_empty() || tag == rtag || tag == "!!str" {
        return Ok((rtag.to_owned(), out));
    }
    if tag == "!!float"
        && let Scalar::Int(int) = out
    {
        return Ok(("!!float".to_owned(), Scalar::Float(int as f64)));
    }
    Err(format!("cannot decode {rtag} as a {tag}"))
}

fn resolve_map(input: &str) -> Option<(&'static str, Scalar)> {
    Some(match input {
        "true" | "True" | "TRUE" => ("!!bool", Scalar::Bool(true)),
        "false" | "False" | "FALSE" => ("!!bool", Scalar::Bool(false)),
        "" | "~" | "null" | "Null" | "NULL" => ("!!null", Scalar::Null),
        ".nan" | ".NaN" | ".NAN" => ("!!float", Scalar::Float(f64::NAN)),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
            ("!!float", Scalar::Float(f64::INFINITY))
        }
        "-.inf" | "-.Inf" | "-.INF" => ("!!float", Scalar::Float(f64::NEG_INFINITY)),
        _ => return None,
    })
}

fn resolve_plain(tag: &str, input: &str) -> (&'static str, Scalar) {
    let hint = match input.as_bytes().first() {
        None => b'N',
        Some(b'+' | b'-') => b'S',
        Some(b'0'..=b'9') => b'D',
        Some(b'y' | b'Y' | b'n' | b'N' | b't' | b'T' | b'f' | b'F' | b'o' | b'O' | b'~') => b'M',
        Some(b'.') => b'.',
        Some(_) => 0,
    };
    if hint != 0 && tag != "!!str" && tag != "!!binary" {
        if let Some(found) = resolve_map(input) {
            return found;
        }
        match hint {
            b'.' => {
                if let Some(float) = parse_dot_float(input) {
                    return ("!!float", Scalar::Float(float));
                }
            }
            b'D' | b'S' => {
                if let Some(found) = resolve_number(tag, input) {
                    return found;
                }
            }
            _ => {}
        }
    }
    ("!!str", Scalar::Str(input.into()))
}

fn resolve_number(tag: &str, input: &str) -> Option<(&'static str, Scalar)> {
    if (tag.is_empty() || tag == "!!timestamp")
        && let Some(time) = parse_timestamp(input)
    {
        return Some(("!!timestamp", Scalar::Timestamp(time)));
    }
    let plain = input.replace('_', "");
    if let Some(int) = parse_int(&plain, 0) {
        return Some(("!!int", Scalar::Int(int)));
    }
    if let Some(uint) = parse_uint(&plain, 0) {
        return Some(("!!int", Scalar::Uint(uint)));
    }
    if is_yaml_style_float(&plain)
        && let Ok(float) = plain.parse::<f64>()
        && float.is_finite()
    {
        return Some(("!!float", Scalar::Float(float)));
    }
    for (prefix, base) in [("0b", 2), ("0o", 8)] {
        if let Some(rest) = plain.strip_prefix(prefix) {
            if let Some(int) = parse_int(rest, base) {
                return Some(("!!int", Scalar::Int(int)));
            }
            if let Some(uint) = parse_uint(rest, base) {
                return Some(("!!int", Scalar::Uint(uint)));
            }
            return None;
        }
        if let Some(rest) = plain
            .strip_prefix('-')
            .and_then(|rest| rest.strip_prefix(prefix))
        {
            return parse_int(&format!("-{rest}"), base).map(|int| ("!!int", Scalar::Int(int)));
        }
    }
    None
}

/// Go's `strconv.ParseUint(s, base, 64)` for the input yaml.v3 gives it
/// (underscores already removed).
fn parse_uint(text: &str, base: u32) -> Option<u64> {
    let bytes = text.as_bytes();
    let (base, digits) = if base == 0 {
        match bytes {
            [] => return None,
            [b'0', marker, _, ..] if marker.eq_ignore_ascii_case(&b'b') => (2, &bytes[2..]),
            [b'0', marker, _, ..] if marker.eq_ignore_ascii_case(&b'o') => (8, &bytes[2..]),
            [b'0', marker, _, ..] if marker.eq_ignore_ascii_case(&b'x') => (16, &bytes[2..]),
            [b'0', rest @ ..] => (8, rest),
            _ => (10, bytes),
        }
    } else {
        if bytes.is_empty() {
            return None;
        }
        (base, bytes)
    };
    let mut value: u64 = 0;
    for &byte in digits {
        let digit = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'z' => u32::from(byte - b'a') + 10,
            b'A'..=b'Z' => u32::from(byte - b'A') + 10,
            _ => return None,
        };
        if digit >= base {
            return None;
        }
        value = value
            .checked_mul(u64::from(base))?
            .checked_add(u64::from(digit))?;
    }
    Some(value)
}

/// Go's `strconv.ParseInt(s, base, 64)`.
fn parse_int(text: &str, base: u32) -> Option<i64> {
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'+') => (false, text.get(1..)?),
        Some(b'-') => (true, text.get(1..)?),
        _ => (false, text),
    };
    let magnitude = parse_uint(digits, base)?;
    if negative {
        if magnitude > 1 << 63 {
            return None;
        }
        Some(0i64.wrapping_sub_unsigned(magnitude))
    } else {
        i64::try_from(magnitude).ok()
    }
}

/// yaml.v3's `yamlStyleFloat`:
/// `^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$`.
fn is_yaml_style_float(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut at = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let digits = |from: usize| {
        bytes
            .iter()
            .skip(from)
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    let integer = digits(at);
    at += integer;
    if integer == 0 {
        if bytes.get(at) != Some(&b'.') {
            return false;
        }
        let fraction = digits(at + 1);
        if fraction == 0 {
            return false;
        }
        at += 1 + fraction;
    } else if bytes.get(at) == Some(&b'.') {
        at += 1 + digits(at + 1);
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let exponent = digits(at);
        if exponent == 0 {
            return false;
        }
        at += exponent;
    }
    at == bytes.len()
}

/// Go's `underscoreOK`: underscores only between digits.
fn underscore_ok(text: &str) -> bool {
    let bytes = text.as_bytes();
    let bytes = match bytes.first() {
        Some(b'+' | b'-') => &bytes[1..],
        _ => bytes,
    };
    let mut saw = b'^';
    let mut start = 0;
    let mut hex = false;
    if let [b'0', marker, ..] = bytes
        && matches!(marker.to_ascii_lowercase(), b'b' | b'o' | b'x')
    {
        start = 2;
        saw = b'0';
        hex = marker.eq_ignore_ascii_case(&b'x');
    }
    for &byte in bytes.iter().skip(start) {
        if byte.is_ascii_digit() || hex && byte.is_ascii_hexdigit() {
            saw = b'0';
            continue;
        }
        if byte == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
            continue;
        }
        if saw == b'_' {
            return false;
        }
        saw = b'!';
    }
    saw != b'_'
}

/// Go's `strconv.ParseFloat` for input starting with `.`.
fn parse_dot_float(input: &str) -> Option<f64> {
    let cleaned = if input.contains('_') {
        if !underscore_ok(input) {
            return None;
        }
        input.replace('_', "")
    } else {
        input.to_owned()
    };
    let bytes = cleaned.as_bytes();
    if bytes.first() != Some(&b'.') || !is_yaml_style_float(&cleaned) {
        return None;
    }
    let float = cleaned.parse::<f64>().ok()?;
    float.is_finite().then_some(float)
}

#[derive(Clone, Copy)]
enum TimestampLayout {
    /// `2006-1-2T15:4:5.999999999Z07:00`, with `T` or `t`.
    DateTime(u8),
    /// `2006-1-2 15:4:5.999999999`.
    Spaced,
    /// `2006-1-2`.
    Date,
}

/// A timestamp's fields, as Go's `time.Parse` reads them. Two are equal
/// when `reflect.DeepEqual` finds the `time.Time`s equal: the same instant
/// in the same zone. A `Z`, or no zone at all, gives UTC; a numeric offset,
/// even `+00:00` or `-00:00`, gives the local zone or a fixed zone with
/// that offset, which never equals UTC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Timestamp {
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    nanosecond: u32,
    /// The numeric zone offset, in seconds east of UTC; `None` for UTC.
    offset: Option<i64>,
}

impl Timestamp {
    /// The time as Go's JSON encoder writes the `time.Time`: RFC 3339, with
    /// as much of the fraction as isn't trailing zeros, and `Z` for a zero
    /// offset. `None` when the encoder refuses it, its zone being 24 hours
    /// or more from UTC.
    pub(crate) fn json_text(&self) -> Option<String> {
        let mut out = self.clock_text();
        let offset = self.offset.unwrap_or(0);
        if offset == 0 {
            out.push('Z');
            return Some(out);
        }
        let minutes = offset.unsigned_abs() / 60;
        let (hours, minutes) = (minutes / 60, minutes % 60);
        if hours >= 24 {
            return None;
        }
        let sign = if offset < 0 { '-' } else { '+' };
        let _ = write!(out, "{sign}{hours:02}:{minutes:02}");
        Some(out)
    }

    /// A timestamp text [`parse_timestamp`] reads back as this one: `Z`
    /// for UTC, and the numeric offset otherwise, which may be up to
    /// `+24:60`.
    pub(crate) fn text(&self) -> String {
        let mut out = self.clock_text();
        let Some(offset) = self.offset else {
            out.push('Z');
            return out;
        };
        let minutes = offset.unsigned_abs() / 60;
        let (mut hours, mut minutes) = (minutes / 60, minutes % 60);
        // 25 hours is the largest offset Go reads, written `24:60`.
        if hours > 24 {
            hours -= 1;
            minutes += 60;
        }
        let sign = if offset < 0 { '-' } else { '+' };
        let _ = write!(out, "{sign}{hours:02}:{minutes:02}");
        out
    }

    /// The date and the clock, with as much of the fraction as isn't
    /// trailing zeros.
    fn clock_text(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        );
        if self.nanosecond != 0 {
            let fraction = format!("{:09}", self.nanosecond);
            out.push('.');
            out.push_str(fraction.trim_end_matches('0'));
        }
        out
    }
}

/// yaml.v3's `parseTimestamp`, using Go's `time.Parse` rules for its four
/// layouts.
pub(crate) fn parse_timestamp(text: &str) -> Option<Timestamp> {
    let bytes = text.as_bytes();
    let year_digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if year_digits != 4 || bytes.get(4) != Some(&b'-') {
        return None;
    }
    [
        TimestampLayout::DateTime(b'T'),
        TimestampLayout::DateTime(b't'),
        TimestampLayout::Spaced,
        TimestampLayout::Date,
    ]
    .into_iter()
    .find_map(|layout| parse_layout(bytes, layout))
}

/// Go's `getnum` without the fixed flag: one or two digits.
fn get_number(bytes: &[u8]) -> Option<(u32, &[u8])> {
    match bytes {
        [a, b, rest @ ..] if a.is_ascii_digit() && b.is_ascii_digit() => {
            Some((u32::from(a - b'0') * 10 + u32::from(b - b'0'), rest))
        }
        [a, rest @ ..] if a.is_ascii_digit() => Some((u32::from(a - b'0'), rest)),
        _ => None,
    }
}

fn two_digits(bytes: &[u8]) -> Option<u32> {
    match bytes {
        [a, b] if a.is_ascii_digit() && b.is_ascii_digit() => {
            Some(u32::from(a - b'0') * 10 + u32::from(b - b'0'))
        }
        _ => None,
    }
}

fn days_in(month: u32, year: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn parse_layout(bytes: &[u8], layout: TimestampLayout) -> Option<Timestamp> {
    let year = bytes
        .get(..4)?
        .iter()
        .fold(0u32, |acc, b| acc * 10 + u32::from(b - b'0'));
    let rest = bytes.get(5..)?;
    let (month, rest) = get_number(rest)?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let rest = rest.strip_prefix(b"-")?;
    let (day, mut rest) = get_number(rest)?;
    let mut time = Timestamp {
        year,
        month,
        day,
        ..Timestamp::default()
    };
    match layout {
        TimestampLayout::Date => {}
        TimestampLayout::DateTime(separator) => {
            rest = rest.strip_prefix(&[separator])?;
            rest = parse_clock(rest, &mut time)?;
            if let Some(after) = rest.strip_prefix(b"Z") {
                rest = after;
            } else {
                let zone = rest.get(..6)?;
                if zone.get(3) != Some(&b':') || !matches!(zone.first(), Some(b'+' | b'-')) {
                    return None;
                }
                let hours = two_digits(zone.get(1..3)?)?;
                let minutes = two_digits(zone.get(4..6)?)?;
                if hours > 24 || minutes > 60 {
                    return None;
                }
                let offset = i64::from(hours * 3600 + minutes * 60);
                time.offset = Some(if zone.first() == Some(&b'-') {
                    -offset
                } else {
                    offset
                });
                rest = rest.get(6..)?;
            }
        }
        TimestampLayout::Spaced => {
            if let Some(first) = rest.first() {
                if *first != b' ' {
                    return None;
                }
                let spaces = rest.iter().take_while(|b| **b == b' ').count();
                rest = rest.get(spaces..)?;
            }
            rest = parse_clock(rest, &mut time)?;
        }
    }
    if !rest.is_empty() || day < 1 || day > days_in(month, year) {
        return None;
    }
    Some(time)
}

/// `15:4:5.999999999`: hour, minute, second and an optional fraction, of
/// which Go keeps nine digits.
fn parse_clock<'a>(bytes: &'a [u8], time: &mut Timestamp) -> Option<&'a [u8]> {
    let (hour, rest) = get_number(bytes)?;
    let rest = rest.strip_prefix(b":")?;
    let (minute, rest) = get_number(rest)?;
    let rest = rest.strip_prefix(b":")?;
    let (second, mut rest) = get_number(rest)?;
    if hour >= 24 || minute >= 60 || second >= 60 {
        return None;
    }
    if let [b'.' | b',', digit, ..] = rest
        && digit.is_ascii_digit()
    {
        let digits = rest
            .iter()
            .skip(1)
            .take_while(|b| b.is_ascii_digit())
            .count();
        let kept = digits.min(9);
        let fraction = rest
            .iter()
            .skip(1)
            .take(kept)
            .fold(0u32, |acc, b| acc * 10 + u32::from(b - b'0'));
        time.nanosecond = (kept..9).fold(fraction, |acc, _| acc * 10);
        rest = rest.get(1 + digits..)?;
    }
    (time.hour, time.minute, time.second) = (hour, minute, second);
    Some(rest)
}

/// Go's `base64.StdEncoding.DecodeString`, which skips CR and LF.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        Some(u32::from(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    }
    let bytes: Vec<u8> = text
        .bytes()
        .filter(|b| *b != b'\r' && *b != b'\n')
        .collect();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let quanta = bytes.len() / 4;
    let mut out = Vec::with_capacity(quanta * 3);
    for (index, quantum) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let padding = quantum.iter().position(|b| *b == b'=');
        let used = match padding {
            None => 4,
            Some(at) => {
                if index + 1 != quanta || at < 2 || quantum.iter().skip(at).any(|b| *b != b'=') {
                    return None;
                }
                at
            }
        };
        let mut word = 0u32;
        for (at, byte) in quantum.iter().enumerate() {
            let bits = if at < used { sextet(*byte)? } else { 0 };
            word = (word << 6) | bits;
        }
        let [_, first, second, third] = word.to_be_bytes();
        out.push(first);
        if used > 2 {
            out.push(second);
        }
        if used > 3 {
            out.push(third);
        }
    }
    Some(out)
}

/// Go's `strconv.Quote`, as `%#v` prints strings.
pub(crate) fn go_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            _ if (c as u32) < 0x20 || c as u32 == 0x7F => {
                out.push('\\');
                let _ = write!(out, "x{:02x}", c as u32);
            }
            _ if is_go_printable(c) => out.push(c),
            _ if (c as u32) < 0x10000 => {
                out.push('\\');
                let _ = write!(out, "u{:04x}", c as u32);
            }
            _ => {
                out.push('\\');
                let _ = write!(out, "U{:08x}", c as u32);
            }
        }
    }
    out.push('"');
    out
}

fn is_go_printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    if c.is_control() || c.is_whitespace() {
        return false;
    }
    !matches!(c as u32, 0xAD | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x2064 | 0xFEFF)
}

// ---------------------------------------------------------------------------
// The shape pass

/// yaml.v3's duplicate-key errors for a mapping, in its order.
pub(crate) fn duplicate_key_errors(node: &Node) -> Vec<String> {
    // Keys cut short by excessive aliasing fail when decoded instead.
    let keys: Vec<&Node> = node
        .content
        .iter()
        .step_by(2)
        .filter(|key| key.kind != Kind::Poison)
        .collect();
    let mut groups: HashMap<(u8, &str), Vec<usize>> = HashMap::new();
    for (index, key) in keys.iter().enumerate() {
        groups.entry(key.go_identity()).or_default().push(index);
    }
    let mut errors = Vec::new();
    for (index, key) in keys.iter().enumerate() {
        let Some(group) = groups.get(&key.go_identity()) else {
            continue;
        };
        let later = group.iter().filter(|other| **other > index);
        for &other in later {
            let Some(duplicate) = keys.get(other) else {
                continue;
            };
            let (_, value) = duplicate.go_identity();
            errors.push(format!(
                "line {}: mapping key {} already defined at line {}",
                duplicate.go_line(),
                go_quote(value),
                key.go_line()
            ));
            if errors.len() >= MAX_TYPE_ERRORS {
                return errors;
            }
        }
    }
    errors
}

/// yaml.v3's `isMerge`.
pub(crate) fn is_merge(key: &Node) -> bool {
    key.kind == Kind::Scalar && key.alias.is_none() && key.value == "<<" && key.tag == "!!merge"
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyTarget {
    /// Keys decode into Go strings.
    Str,
    /// Keys decode into `interface{}`.
    Any,
}

struct Shape {
    errors: Vec<String>,
    merged: Option<HashSet<String>>,
}

/// Runs upstream's `node.Decode(&map[string]any)` check: duplicate keys,
/// merge rules, alias cycles and scalars that can't be resolved. The root
/// must be a mapping.
pub(crate) fn check_shape(root: &Node) -> Result<(), YamlError> {
    let mut shape = Shape {
        errors: Vec::new(),
        merged: None,
    };
    shape.mapping(root, KeyTarget::Str)?;
    if !shape.errors.is_empty() {
        return Err(YamlError::Type(shape.errors));
    }
    match first_poison(root) {
        Some(message) => Err(YamlError::Fatal(message.to_owned())),
        None => Ok(()),
    }
}

/// Runs yaml.v3's decode of `node` into Go's `any` (`node.Decode(&v)` with
/// `var v any`): duplicate keys, merge rules, alias cycles, excessive
/// aliasing and scalars that can't be resolved, anywhere in the tree.
pub(crate) fn check_decode_any(node: &Node) -> Result<(), YamlError> {
    let mut shape = Shape {
        errors: Vec::new(),
        merged: None,
    };
    shape.value(node)?;
    if !shape.errors.is_empty() {
        return Err(YamlError::Type(shape.errors));
    }
    match first_poison(node) {
        Some(message) => Err(YamlError::Fatal(message.to_owned())),
        None => Ok(()),
    }
}

fn first_poison(node: &Node) -> Option<&str> {
    if node.kind == Kind::Poison {
        return Some(node.value.as_str());
    }
    node.content.iter().find_map(first_poison)
}

fn is_string_map(node: &Node) -> bool {
    node.content
        .iter()
        .step_by(2)
        .all(|key| matches!(key.go_short_tag(), "!!str" | "!!merge"))
}

impl Shape {
    fn value(&mut self, node: &Node) -> Result<(), YamlError> {
        match node.kind {
            Kind::Poison => Err(YamlError::Fatal(node.value.to_string())),
            Kind::Scalar => resolve_node(node).map(|_| ()),
            Kind::Sequence => node.content.iter().try_for_each(|item| self.value(item)),
            Kind::Mapping => {
                let target = if is_string_map(node) {
                    KeyTarget::Str
                } else {
                    KeyTarget::Any
                };
                self.mapping(node, target)
            }
        }
    }

    /// The key as decoded, for merge bookkeeping; `None` when yaml.v3 skips it.
    fn key(&mut self, key: &Node, target: KeyTarget) -> Result<Option<String>, YamlError> {
        match (key.kind, target) {
            (Kind::Poison, _) => Err(YamlError::Fatal(key.value.to_string())),
            (Kind::Scalar, KeyTarget::Str) => {
                Ok(scalar_string(key)?.map(|text| format!("s:{text}")))
            }
            (Kind::Scalar, KeyTarget::Any) => {
                let resolved = resolve_node(key)?;
                Ok(Some(match resolved.value {
                    Scalar::Null => "n".to_owned(),
                    Scalar::Bool(value) => format!("b:{value}"),
                    Scalar::Int(value) => format!("i:{value}"),
                    Scalar::Uint(value) => format!("u:{value}"),
                    Scalar::Float(value) => format!("f:{}", value.to_bits()),
                    Scalar::Timestamp(_) => format!("t:{}", key.value),
                    Scalar::Str(text) => format!("s:{text}"),
                }))
            }
            (_, KeyTarget::Str) => {
                if key.kind == Kind::Mapping {
                    let duplicates = duplicate_key_errors(key);
                    if !duplicates.is_empty() {
                        duplicates
                            .into_iter()
                            .for_each(|e| push_type_error(&mut self.errors, e));
                        return Ok(None);
                    }
                }
                push_type_error(&mut self.errors, type_error(key, "string"));
                Ok(None)
            }
            (_, KeyTarget::Any) => {
                self.value(key)?;
                Err(YamlError::Fatal("invalid map key".to_owned()))
            }
        }
    }

    fn mapping(&mut self, node: &Node, target: KeyTarget) -> Result<(), YamlError> {
        let duplicates = duplicate_key_errors(node);
        if !duplicates.is_empty() {
            duplicates
                .into_iter()
                .for_each(|e| push_type_error(&mut self.errors, e));
            return Ok(());
        }
        let mut merged = self.merged.take();
        let mut merge_node = None;
        for (key, value) in node.pairs() {
            if is_merge(key) {
                merge_node = Some(value);
                continue;
            }
            let Some(decoded) = self.key(key, target)? else {
                continue;
            };
            if let Some(seen) = merged.as_mut()
                && !seen.insert(decoded)
            {
                continue;
            }
            self.value(value)?;
        }
        self.merged = merged;
        match merge_node {
            Some(merge) => self.merge(node, merge, target),
            None => Ok(()),
        }
    }

    fn merge(&mut self, parent: &Node, merge: &Node, target: KeyTarget) -> Result<(), YamlError> {
        let fresh = self.merged.is_none();
        if fresh {
            let mut seen = HashSet::new();
            for key in parent.content.iter().step_by(2) {
                if let Some(decoded) = self.key(key, KeyTarget::Any)? {
                    seen.insert(decoded);
                }
            }
            self.merged = Some(seen);
        }
        let result = self.merge_value(merge, target);
        if fresh {
            self.merged = None;
        }
        result
    }

    fn merge_value(&mut self, merge: &Node, target: KeyTarget) -> Result<(), YamlError> {
        if merge.kind == Kind::Poison {
            return Err(YamlError::Fatal(merge.value.to_string()));
        }
        if merge.alias.is_some() || merge.kind == Kind::Mapping {
            if merge.kind != Kind::Mapping {
                return Err(YamlError::Fatal(WANT_MAP.to_owned()));
            }
            return self.mapping(merge, target);
        }
        if merge.kind != Kind::Sequence {
            return Err(YamlError::Fatal(WANT_MAP.to_owned()));
        }
        for item in &merge.content {
            if item.kind == Kind::Poison {
                return Err(YamlError::Fatal(item.value.to_string()));
            }
            if item.kind != Kind::Mapping {
                return Err(YamlError::Fatal(WANT_MAP.to_owned()));
            }
            self.mapping(item, target)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tree edits (config_v8.go)

/// Upstream's `expandConfigAliases`: a copy with alias marks dropped and
/// merge keys folded into their mappings, earlier keys winning.
pub(crate) fn expand_merges(node: &Node) -> Node {
    let mut copy = Node {
        kind: node.kind,
        tag: node.tag.clone(),
        value: node.value.clone(),
        content: node.content.iter().map(expand_merges).collect(),
        line: node.line,
        alias: None,
        flow: node.flow,
        tagged: node.tagged,
        aliased: node.aliased,
    };
    if copy.kind != Kind::Mapping {
        return copy;
    }
    let mut index = 0;
    while index + 1 < copy.content.len() {
        if copy
            .content
            .get(index)
            .is_none_or(|key| key.tag != "!!merge")
        {
            index += 2;
            continue;
        }
        // The loop condition keeps index + 1 in bounds.
        let pair: Vec<Node> = copy.content.drain(index..index + 2).collect();
        let merge = pair.into_iter().nth(1).unwrap_or_default();
        let mappings = if merge.kind == Kind::Sequence {
            merge.content
        } else {
            vec![merge]
        };
        for mapping in mappings {
            let mut items = mapping.content.into_iter();
            while let (Some(key), Some(value)) = (items.next(), items.next()) {
                if find_map_key_index(&copy, &key.value).is_none() {
                    copy.content.push(key);
                    copy.content.push(value);
                }
            }
        }
    }
    copy
}

/// The index of `key` in a mapping's content.
pub(crate) fn find_map_key_index(node: &Node, key: &str) -> Option<usize> {
    if node.kind != Kind::Mapping {
        return None;
    }
    (0..node.content.len()).step_by(2).find(|&index| {
        index + 1 < node.content.len() && node.content.get(index).is_some_and(|k| k.value == key)
    })
}

/// The node at a dotted path.
pub(crate) fn yaml_path<'a>(node: &'a Node, path: &str) -> Option<&'a Node> {
    let mut current = node;
    for part in path.split('.') {
        let index = find_map_key_index(current, part)?;
        current = current.content.get(index + 1)?;
    }
    Some(current)
}

/// Upstream's `getOrCreateMapValue`.
fn get_or_create_map_value<'a>(node: &'a mut Node, key: &str) -> &'a mut Node {
    if node.kind != Kind::Mapping {
        node.kind = Kind::Mapping;
        node.tag = "!!map".into();
        node.content.clear();
    }
    let index = match find_map_key_index(node, key) {
        Some(index) => index + 1,
        None => {
            node.content.push(Node::scalar("!!str", key));
            node.content.push(Node::scalar("!!str", ""));
            node.content.len() - 1
        }
    };
    // In bounds: find_map_key_index only returns a key with a value after it.
    &mut node.content[index]
}

/// Upstream's `setYAMLPath`: creates mappings along the way.
pub(crate) fn set_yaml_path(root: &mut Node, path: &str, value: &Node) {
    let mut current = root;
    for part in path.split('.') {
        current = get_or_create_map_value(current, part);
    }
    *current = value.clone();
}

/// Upstream's `deleteYAMLPath`: removes the key, and parents it leaves empty.
pub(crate) fn delete_yaml_path(node: &mut Node, path: &str) -> bool {
    let (key, rest) = match path.split_once('.') {
        Some((key, rest)) => (key, Some(rest)),
        None => (path, None),
    };
    let Some(index) = find_map_key_index(node, key) else {
        return false;
    };
    if let Some(rest) = rest {
        let Some(child) = node.content.get_mut(index + 1) else {
            return false;
        };
        if !delete_yaml_path(child, rest) {
            return false;
        }
        if !child.content.is_empty() {
            return true;
        }
    }
    node.content
        .drain(index..(index + 2).min(node.content.len()));
    true
}

// ---------------------------------------------------------------------------
// Writing out and reading back (encode.go, emitterc.go)

/// Gives a tree the tags it has after yaml.v3 writes it out and reads it
/// back, as upstream does with a migrated v8 config (`yaml.Marshal`, then
/// `yaml.Unmarshal`). Values come back as they were. A scalar whose tag
/// isn't written out, being the one its value resolves to, comes back a
/// string when the emitter quotes it: an empty mapping key or flow value,
/// and a timestamp with a `:` in flow style.
pub(crate) fn write_and_read_back(node: &mut Node) {
    write_node(node, false, false);
}

/// [`write_and_read_back`] for a node in flow style (`in_flow`) or as a
/// mapping key (`key`).
fn write_node(node: &mut Node, in_flow: bool, key: bool) {
    match node.kind {
        Kind::Scalar => {
            // encoder.node drops the tag; the emitter quotes the value if
            // it can't be plain, and a quoted value reads back as a string.
            let implicit =
                !node.tagged && node.tag != "!!str" && node.tag == resolve_plain("", &node.value).0;
            if implicit && !written_plain(&node.value, in_flow, key) {
                node.tag = "!!str".into();
            }
        }
        Kind::Sequence | Kind::Mapping => {
            let in_flow = in_flow || node.flow;
            let mapping = node.kind == Kind::Mapping;
            for (index, child) in node.content.iter_mut().enumerate() {
                write_node(child, in_flow, mapping && index % 2 == 0);
            }
        }
        Kind::Poison => {}
    }
}

/// Whether yaml.v3's emitter writes `value` as a plain scalar where a
/// plain scalar was asked for (`yaml_emitter_select_scalar_style`, with
/// `yaml_emitter_analyze_scalar`'s analysis). A key is always a simple key
/// here: one that isn't is never empty.
fn written_plain(value: &str, in_flow: bool, key: bool) -> bool {
    if value.is_empty() {
        return !in_flow && !key;
    }
    let mut flow_indicators = value.starts_with("---") || value.starts_with("...");
    let mut block_indicators = flow_indicators;
    let mut not_plain = false;
    let mut preceded_by_whitespace = true;
    let mut chars = value.chars().peekable();
    let mut first = true;
    while let Some(c) = chars.next() {
        let followed_by_whitespace = matches!(chars.peek(), None | Some(' ' | '\t'));
        match (first, c) {
            (true, '#' | ',' | '[' | ']' | '{' | '}' | '&' | '*' | '!' | '|' | '>')
            | (true, '\'' | '"' | '%' | '@' | '`') => {
                flow_indicators = true;
                block_indicators = true;
            }
            (true, '?') | (_, ':') => {
                flow_indicators = true;
                block_indicators |= followed_by_whitespace;
            }
            (true, '-') => {
                flow_indicators |= followed_by_whitespace;
                block_indicators |= followed_by_whitespace;
            }
            (false, ',' | '?' | '[' | ']' | '{' | '}') => flow_indicators = true,
            (false, '#') if preceded_by_whitespace => {
                flow_indicators = true;
                block_indicators = true;
            }
            _ => {}
        }
        // A tab, a character the emitter escapes, a line break, and space
        // at either end all rule a plain scalar out.
        let printable = matches!(
            u32::from(c),
            0x0A | 0x20..=0x7E | 0xA0..=0xD7FF | 0xE000..=0xFFFD
        ) && c != '\u{FEFF}';
        let line_break = matches!(c, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');
        let space_at_end = c == ' ' && (first || chars.peek().is_none());
        not_plain |= c == '\t' || !printable || line_break || space_at_end;
        preceded_by_whitespace = matches!(c, ' ' | '\t' | '\0') || line_break;
        first = false;
    }
    if in_flow {
        !not_plain && !flow_indicators
    } else {
        !not_plain && !block_indicators
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag_of(value: &str) -> &'static str {
        resolve_plain("", value).0
    }

    #[test]
    fn plain_scalars_resolve_like_yaml_v3() {
        for (input, tag) in [
            ("", "!!null"),
            ("~", "!!null"),
            ("null", "!!null"),
            ("true", "!!bool"),
            ("FALSE", "!!bool"),
            ("yes", "!!str"),
            ("on", "!!str"),
            ("1", "!!int"),
            ("-1", "!!int"),
            ("0x1F", "!!int"),
            ("0o17", "!!int"),
            ("0b101", "!!int"),
            ("0777", "!!int"),
            ("1_000", "!!int"),
            ("08", "!!float"),
            ("1.5", "!!float"),
            (".5", "!!float"),
            ("1e3", "!!float"),
            (".inf", "!!float"),
            ("-.Inf", "!!float"),
            (".nan", "!!float"),
            ("1e400", "!!str"),
            ("9999999999999999999", "!!int"),
            ("99999999999999999999", "!!float"),
            ("2024-01-02", "!!timestamp"),
            ("2024-1-2", "!!timestamp"),
            ("2024-02-30", "!!str"),
            ("2024-02-29", "!!timestamp"),
            ("2023-02-29", "!!str"),
            ("2024-01-02T03:04:05Z", "!!timestamp"),
            ("2024-01-02t03:04:05.123+05:30", "!!timestamp"),
            ("2024-01-02 03:04:05", "!!timestamp"),
            ("2024-01-02   03:04:05.5", "!!timestamp"),
            ("2024-01-02T03:04:05", "!!str"),
            ("2024-01-02T24:00:00Z", "!!str"),
            ("2024-13-02", "!!str"),
            ("20240-01-02", "!!str"),
            ("abc", "!!str"),
            ("+", "!!str"),
            ("0x", "!!str"),
        ] {
            assert_eq!(tag_of(input), tag, "{input:?}");
        }
    }

    /// Not upstream's: a timestamp is written as Go's JSON encoder writes
    /// the `time.Time` yaml.v3 decodes it to.
    #[test]
    fn timestamps_are_written_as_go_writes_them() {
        for (input, want) in [
            ("2002-12-14", Some("2002-12-14T00:00:00Z")),
            (
                "2001-12-14t21:59:43.10-05:00",
                Some("2001-12-14T21:59:43.1-05:00"),
            ),
            ("2024-1-2T3:4:5Z", Some("2024-01-02T03:04:05Z")),
            ("2024-01-02   03:04:05.5", Some("2024-01-02T03:04:05.5Z")),
            ("2024-01-02 03:04:05,25", Some("2024-01-02T03:04:05.25Z")),
            (
                "2024-01-02T03:04:05.1234567891234+05:30",
                Some("2024-01-02T03:04:05.123456789+05:30"),
            ),
            (
                "2024-01-02T03:04:05.000+00:00",
                Some("2024-01-02T03:04:05Z"),
            ),
            ("2024-01-02T03:04:05-00:00", Some("2024-01-02T03:04:05Z")),
            (
                "2024-01-02T03:04:05+23:59",
                Some("2024-01-02T03:04:05+23:59"),
            ),
            ("2024-01-02T03:04:05+24:00", None),
            ("2024-01-02T03:04:05+23:60", None),
            ("0000-02-29", Some("0000-02-29T00:00:00Z")),
            (
                "2024-01-02T03:04:05.000000001Z",
                Some("2024-01-02T03:04:05.000000001Z"),
            ),
            ("2024-01-02T03:04:05", None),
        ] {
            let text = parse_timestamp(input).and_then(|time| time.json_text());
            assert_eq!(text.as_deref(), want, "{input:?}");
        }
    }

    #[test]
    fn numbers_resolve_to_go_values() {
        let value = |input: &str| resolve_plain("", input).1;
        assert_eq!(value("0x1F"), Scalar::Int(31));
        assert_eq!(value("0777"), Scalar::Int(511));
        assert_eq!(value("0o17"), Scalar::Int(15));
        assert_eq!(value("-0o17"), Scalar::Int(-15));
        assert_eq!(value("-0b101"), Scalar::Int(-5));
        assert_eq!(value("0"), Scalar::Int(0));
        assert_eq!(value("-9223372036854775808"), Scalar::Int(i64::MIN));
        assert_eq!(value("9223372036854775808"), Scalar::Uint(1 << 63));
        assert_eq!(value("08"), Scalar::Float(8.0));
        assert_eq!(value("._5"), Scalar::Str("._5".into()));
        assert_eq!(value(".5_5"), Scalar::Float(0.55));
    }

    #[test]
    fn explicit_tags_are_checked() {
        assert_eq!(
            resolve("!!int", "42"),
            Ok(("!!int".to_owned(), Scalar::Int(42)))
        );
        assert_eq!(
            resolve("!!float", "42"),
            Ok(("!!float".to_owned(), Scalar::Float(42.0)))
        );
        assert_eq!(
            resolve("!!int", "abc"),
            Err("cannot decode !!str as a !!int".to_owned())
        );
        assert_eq!(
            resolve("!!str", "42"),
            Ok(("!!str".to_owned(), Scalar::Str("42".into())))
        );
        assert_eq!(
            resolve("!custom", "5"),
            Ok(("!custom".to_owned(), Scalar::Str("5".into())))
        );
    }

    #[test]
    fn base64_follows_go_std_encoding() {
        assert_eq!(decode_base64("aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(decode_base64("aGk="), Some(b"hi".to_vec()));
        assert_eq!(decode_base64("aA=="), Some(b"h".to_vec()));
        assert_eq!(decode_base64("aGVs\nbG8="), Some(b"hello".to_vec()));
        assert_eq!(decode_base64(""), Some(Vec::new()));
        assert_eq!(decode_base64("aGVsbG8"), None);
        assert_eq!(decode_base64("aA=x"), None);
        assert_eq!(decode_base64("a==="), None);
        assert_eq!(decode_base64("aGk=aGk="), None);
        assert_eq!(decode_base64("a!=="), None);
    }

    #[test]
    fn quote_matches_strconv() {
        assert_eq!(go_quote("port"), "\"port\"");
        assert_eq!(go_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(go_quote("tab\there\n"), "\"tab\\there\\n\"");
        assert_eq!(go_quote("\u{1}"), "\"\\x01\"");
        let nbsp = char::from_u32(0xA0).map(String::from).unwrap_or_default();
        assert_eq!(go_quote(&nbsp), format!("\"{}u00a0\"", '\\'));
        assert_eq!(go_quote("caf\u{e9}"), "\"caf\u{e9}\"");
    }

    #[test]
    fn documents_and_aliases_expand() {
        let Ok(Some(root)) = parse_document("a: &x [1, 2]\nb: *x\n") else {
            panic!("expected a document");
        };
        let b = yaml_path(&root, "b").cloned().unwrap_or_default();
        assert_eq!(b.kind, Kind::Sequence);
        assert_eq!(b.content.len(), 2);
        assert_eq!(b.alias.as_ref().map(|a| a.name.as_str()), Some("x"));
        assert_eq!(parse_document(""), Ok(None));
        assert_eq!(parse_document("# only a comment\n"), Ok(None));
        // yaml.v3 refuses these characters wherever they are; the rest are
        // read as they are.
        let refused = Err(YamlError::Syntax(
            "yaml: control characters are not allowed".to_owned(),
        ));
        for text in [
            "port: 1235\n\0api-keys: [new]\n",
            "port: 1\u{1}\n",
            "port: 1\u{7f}\n",
            "port: \"a\u{80}b\"\n",
            "port: \"a\u{ffff}b\"\n",
            "a: |\n  x\n  y\u{c}\n",
            "port: 1\n---\nb: \0\n",
        ] {
            assert_eq!(parse_document(text), refused, "{text:?}");
        }
        for text in [
            "a: \"x\u{85}y\"\n",
            "a: \"x\u{feff}y\"\n",
            "a: 'x\ty'\n",
            "a: x\r\n",
        ] {
            assert!(matches!(parse_document(text), Ok(Some(_))), "{text:?}");
        }
        assert_eq!(
            parse_document("a: *nope\n"),
            Err(YamlError::Syntax(
                "yaml: unknown anchor 'nope' referenced".to_owned()
            ))
        );
    }

    #[test]
    fn deep_nesting_is_rejected() {
        let deep = format!(
            "{}x
",
            "- ".repeat(MAX_DEPTH + 5)
        );
        let Err(YamlError::Syntax(message)) = parse_document(&deep) else {
            panic!("expected a depth error");
        };
        assert_eq!(message, "yaml: line 1: exceeded max depth of 256");
        let shallow = format!(
            "{}x
",
            "- ".repeat(MAX_DEPTH - 1)
        );
        assert!(parse_document(&shallow).is_ok());
    }

    #[test]
    fn alias_bombs_are_stopped() {
        let mut text = String::from("a0: &a0 [x, x, x, x, x, x, x, x, x, x]\n");
        for level in 1..9 {
            let previous = level - 1;
            let refs = vec![format!("*a{previous}"); 10].join(", ");
            text.push_str(&format!("a{level}: &a{level} [{refs}]\n"));
        }
        let Ok(Some(root)) = parse_document(&text) else {
            panic!("expected a document");
        };
        assert_eq!(
            check_shape(&root),
            Err(YamlError::Fatal(
                "document contains excessive aliasing".to_owned()
            ))
        );
    }

    /// The tree written as a flow collection, keys in document order.
    fn dump(node: &Node) -> String {
        let items: Vec<String> = match node.kind {
            Kind::Scalar => return node.value.to_string(),
            Kind::Poison => return "!".to_owned(),
            Kind::Sequence => node.content.iter().map(dump).collect(),
            Kind::Mapping => node
                .pairs()
                .map(|(key, value)| format!("{}: {}", dump(key), dump(value)))
                .collect(),
        };
        match node.kind {
            Kind::Sequence => format!("[{}]", items.join(", ")),
            _ => format!("{{{}}}", items.join(", ")),
        }
    }

    /// Not upstream's: an alias expands to copies that share its anchor's
    /// text, so a long scalar aliased many times (here 64 KiB, 800 times,
    /// under a key nothing reads) takes its length in memory once, through
    /// merges and clones too.
    #[test]
    fn aliases_share_their_anchor_text() {
        let long = "x".repeat(64 << 10);
        let aliases = vec!["*big"; 800].join(", ");
        let text = format!("unused:\n  anchor: &big {long}\n  copies: [{aliases}]\n");
        let Ok(Some(root)) = parse_document(&text) else {
            panic!("expected a document");
        };
        assert_eq!(check_shape(&root), Ok(()));
        let merged = expand_merges(&root.clone());
        for tree in [&root, &merged] {
            let Some(anchor) = yaml_path(tree, "unused.anchor") else {
                panic!("the anchor is there");
            };
            let Some(copies) = yaml_path(tree, "unused.copies") else {
                panic!("the copies are there");
            };
            assert_eq!(copies.content.len(), 800);
            assert!(!anchor.aliased);
            for copy in &copies.content {
                assert!(copy.aliased);
                assert!(copy.value.shares(&anchor.value));
            }
        }
        // A copy decoded into an owned value is counted.
        let budget = AliasBudget::default();
        for copy in yaml_path(&root, "unused.copies").map_or(&[][..], |n| &n.content) {
            assert_eq!(budget.charge(copy), Ok(()));
        }
        assert_eq!(budget.0.get(), 800 * long.len());
        assert_eq!(budget.charge(&Node::scalar("!!str", &long)), Ok(()));
        assert_eq!(budget.0.get(), 800 * long.len());
        let mut copy = Node::scalar("!!str", &"y".repeat(MAX_ALIASED_BYTES));
        copy.aliased = true;
        assert_eq!(
            budget.charge(&copy),
            Err(YamlError::Fatal(EXCESSIVE_ALIASING.to_owned()))
        );
    }

    /// Not upstream's: a node's `Debug` shows its kind, line and size,
    /// never its text.
    #[test]
    fn debug_leaves_out_text() {
        let Ok(Some(root)) = parse_document("secret-key: &s marker-secret-41\ncopy: *s\n") else {
            panic!("expected a document");
        };
        let Ok(resolved) = resolve_node(&Node::scalar("!!str", "marker-secret-41")) else {
            panic!("a string resolves");
        };
        for shown in [
            format!("{root:?}"),
            format!("{:?}", root.content),
            format!("{resolved:?}"),
            format!("{:?}", resolve_plain("", "marker-secret-41")),
        ] {
            assert!(!shown.contains("marker-secret-41"), "{shown}");
        }
        assert_eq!(
            format!("{root:?}"),
            "Node { kind: Mapping, line: 1, content: 4, .. }"
        );
    }

    /// Not upstream's: an alias used as a key with the `:` right after its
    /// name reads as yaml.v3 reads it. Recorded from yaml.v3 v3.0.1 under
    /// Go 1.26.4.
    #[test]
    fn alias_keys_read_as_yaml_v3_reads_them() {
        let read = |text: &str| parse_document(text).map(|root| root.as_ref().map(dump));
        for (text, want) in [
            (
                "k: &k server\n*k: {port: 14}\n",
                "{k: server, server: {port: 14}}",
            ),
            (
                "k: &k server\n*k:\n  port: 14\n",
                "{k: server, server: {port: 14}}",
            ),
            (
                "k: &k server\nx:\n  - *k: 1\n",
                "{k: server, x: [{server: 1}]}",
            ),
            ("k: &k server\nx:\n  *k: 1\n", "{k: server, x: {server: 1}}"),
            (
                "k: &k server\nx: {*k: 1, b: *k}\n",
                "{k: server, x: {server: 1, b: server}}",
            ),
            (
                "k: &k server\nx: [*k: 1]\n",
                "{k: server, x: [{server: 1}]}",
            ),
        ] {
            assert_eq!(read(text), Ok(Some(want.to_owned())), "{text:?}");
        }
        assert_eq!(
            read("*nope: 1\n"),
            Err(YamlError::Syntax(
                "yaml: unknown anchor 'nope' referenced".to_owned()
            ))
        );
        let Ok(Some(twice)) = parse_document("k: &k server\n*k: 1\n*k: 2\n") else {
            panic!("expected a document");
        };
        assert_eq!(
            check_shape(&twice),
            Err(YamlError::Type(vec![
                "line 3: mapping key \"k\" already defined at line 2".to_owned()
            ]))
        );
        // yaml.v3 refuses these too.
        for text in [
            "k: &k server\nx: *k:\n",
            "k: &k server\n*k:: 1\n",
            "k: &k server\n*k::1\n",
            "k: &k server\n*k:x: 1\n",
        ] {
            assert!(read(text).is_err(), "{text:?}");
        }
        // yaml.v3 reads these, with the `:` followed by something else;
        // saphyr refuses a tab after any key's `:`.
        for text in [
            "k: &k server\nx: {*k:1}\n",
            "k: &k server\nx: [*k:]\n",
            "k: &k server\n*k:\t1\n",
            "k:\t1\n",
        ] {
            assert!(read(text).is_err(), "{text:?}");
        }
        // A `*k: ` that isn't an alias is left as it is.
        assert_eq!(
            read("k: &k server\n*k: \"*k: x\" # *k: y\nz: |\n  *k: z\n"),
            Ok(Some("{k: server, server: *k: x, z: *k: z\n}".to_owned()))
        );
        // However many there are.
        let anchors: String = (0..200).map(|n| format!("a{n}: &a{n} v{n}\n")).collect();
        let keys: String = (0..200).map(|n| format!("*a{n}: {n}\n")).collect();
        let Ok(Some(root)) = parse_document(&format!("{anchors}{keys}")) else {
            panic!("expected a document");
        };
        assert_eq!(root.content.len(), 800);
        assert_eq!(
            yaml_path(&root, "v199").map(|n| n.value.as_str()),
            Some("199")
        );
    }

    /// Not upstream's: the tags a tree has after yaml.v3 writes it out and
    /// reads it back. Recorded from yaml.v3 v3.0.1 under Go 1.26.4.
    #[test]
    fn written_out_and_read_back_as_yaml_v3_does() {
        let tags = |text: &str| {
            let Ok(Some(mut root)) = parse_document(text) else {
                panic!("expected a document: {text:?}");
            };
            write_and_read_back(&mut root);
            let mut tags = Vec::new();
            let mut stack = vec![&root];
            while let Some(node) = stack.pop() {
                if node.kind == Kind::Scalar {
                    tags.push(node.tag.to_string());
                }
                stack.extend(node.content.iter().rev());
            }
            tags
        };
        for (text, want) in [
            // A timestamp with a `:` can't be plain in flow style.
            (
                "v: 2001-12-14t21:59:43.10-05:00\n",
                &["!!str", "!!timestamp"][..],
            ),
            ("{v: 2001-12-14t21:59:43.10-05:00}\n", &["!!str", "!!str"]),
            ("a: [2001-12-14 21:59:43.10]\n", &["!!str", "!!str"]),
            ("a: [2002-12-14]\n", &["!!str", "!!timestamp"]),
            (
                "a: &t {v: 2001-12-14T21:59:43Z}\nb: *t\n",
                &["!!str", "!!str", "!!str", "!!str", "!!str", "!!str"],
            ),
            // An empty value is quoted in flow style or as a key.
            ("{v: }\n", &["!!str", "!!str"]),
            ("v:\n", &["!!str", "!!null"]),
            ("- \n- ~\n", &["!!null", "!!null"]),
            ("? \n: x\n", &["!!str", "!!str"]),
            ("{~: x}\n", &["!!null", "!!str"]),
            ("[a: ]\n", &["!!str", "!!str"]),
            // Written tags and other values are kept.
            ("{v: !!null }\n", &["!!str", "!!null"]),
            (
                "{v: !!timestamp 2002-12-14T01:02:03Z}\n",
                &["!!str", "!!timestamp"],
            ),
            (
                "{a: 1, b: -.inf, c: true, d: 0x1F}\n",
                &[
                    "!!str", "!!int", "!!str", "!!float", "!!str", "!!bool", "!!str", "!!int",
                ],
            ),
        ] {
            assert_eq!(tags(text), want, "{text:?}");
        }
        for (value, flow, block) in [
            ("a", true, true),
            ("a:b", false, true),
            ("a: b", false, false),
            ("a,b", false, true),
            ("-a", true, true),
            ("- a", false, false),
            ("---a", false, false),
            ("a #b", false, false),
            ("a#b", true, true),
            ("#a", false, false),
            (" a", false, false),
            ("a ", false, false),
            ("a\tb", false, false),
            ("a\nb", false, false),
            ("caf\u{e9}", true, true),
            ("\u{1F600}", false, false),
            ("?a", false, true),
            (":", false, false),
        ] {
            assert_eq!(written_plain(value, true, false), flow, "{value:?} in flow");
            assert_eq!(
                written_plain(value, false, false),
                block,
                "{value:?} in block"
            );
        }
    }

    #[test]
    fn path_edits_follow_upstream() {
        let mut root = Node::mapping();
        set_yaml_path(&mut root, "a.b.c", &Node::scalar("!!int", "1"));
        assert_eq!(
            yaml_path(&root, "a.b.c").map(|n| n.value.as_str()),
            Some("1")
        );
        set_yaml_path(&mut root, "a.d", &Node::scalar("!!int", "2"));
        assert!(delete_yaml_path(&mut root, "a.b.c"));
        assert!(yaml_path(&root, "a.b").is_none());
        assert!(yaml_path(&root, "a.d").is_some());
        assert!(!delete_yaml_path(&mut root, "a.zz"));
        assert!(delete_yaml_path(&mut root, "a.d"));
        assert!(yaml_path(&root, "a").is_none());
    }
}
