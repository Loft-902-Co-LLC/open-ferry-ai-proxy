// Ported from gopkg.in/yaml.v3 v3.0.1 decode.go (parser: newParser, init,
// expect, peek, fail, anchor, parse, node, parseChild, document, alias,
// scalar, sequence, mapping) and yaml.go (Unmarshal into a yaml.Node)
// (Apache-2.0), the YAML library CLIProxyAPI v8.0.20 (MIT) reads and writes
// its config with.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/go-yaml/yaml
//
// Copyright (c) 2011-2019 Canonical Ltd
// Licensed under the Apache License, Version 2.0; see licenses/go-yaml-LICENSE
// and licenses/go-yaml-NOTICE.

//! Builds a [`Node`] tree from the parser's events, as `yaml.Unmarshal`
//! into a `yaml.Node` does: the first document, with its comments moved
//! where yaml.v3 moves them.
//!
//! Deviations from upstream:
//! - An alias holds a copy of its anchor's node as the finished tree has
//!   it ([`AliasTarget::Node`]), where yaml.v3's points to the node itself;
//!   an alias inside its anchor's node holds [`AliasTarget::Cycle`]. The
//!   copies of anchors that later aliases name may add up to at most
//!   [`MAX_ALIAS_COPIES`] nodes; past that the document is refused with
//!   `yaml: document contains excessive aliasing`, the error yaml.v3 gives
//!   when it decodes such a document.
//! - Nesting deeper than [`MAX_DEPTH`] is refused (`yaml: line N: exceeded
//!   max depth of 256`); yaml.v3 allows 10000 levels.
//! - yaml.v3 panics on an event its composer doesn't expect, which its
//!   parser never produces; this returns an error.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use super::parser::Parser;
use super::types::{CollectionStyle, Event, EventType, ParserError, ScalarStyle};
use super::{AliasTarget, Kind, MAP_TAG, MAX_DEPTH, MERGE_TAG, Node, SEQ_TAG, STR_TAG, Style};

/// The most nodes the copies of aliased anchors may add up to.
pub(crate) const MAX_ALIAS_COPIES: usize = 1 << 20;

/// A YAML error, worded as yaml.v3 words it (`yaml: ...`).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct YamlError(pub(crate) String);

impl YamlError {
    /// An error from yaml.v3's `failf`: the message after `yaml: `.
    pub(crate) fn fail(message: impl fmt::Display) -> Self {
        Self(format!("yaml: {message}"))
    }

    /// The error's text.
    pub(crate) fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for YamlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for YamlError {
    // yaml.v3's messages quote a document's keys and anchors, never its
    // values, so the text is safe to show.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("YamlError").field(&self.0).finish()
    }
}

impl std::error::Error for YamlError {}

impl From<ParserError> for YamlError {
    fn from(error: ParserError) -> Self {
        Self::fail(error.message())
    }
}

/// `yaml.Unmarshal(data, &node)`: the document node of the first document,
/// or a zero [`Node`] when the input holds none.
pub(crate) fn unmarshal(data: &[u8]) -> Result<Node, YamlError> {
    // newParser: an empty input reads as a line break.
    let input: &[u8] = if data.is_empty() { b"\n" } else { data };
    let mut composer = Composer {
        parser: Parser::new(input),
        event: Event::default(),
        anchors: HashMap::new(),
        next_anchor: 0,
        path: Vec::new(),
        aliases: Vec::new(),
    };
    composer.init()?;
    let Some(mut root) = composer.parse()? else {
        return Ok(Node::default());
    };
    resolve_aliases(&mut root, &composer.aliases)?;
    Ok(root)
}

/// Where an anchor's node is, by the indices that lead to it from the
/// document's top node.
type Path = Vec<usize>;

/// An anchor as the composer last saw it defined.
struct Anchor {
    /// Which definition this is, so the end of an outer node with the same
    /// anchor doesn't take the name back.
    id: usize,
    path: Path,
    /// Whether the node has ended.
    done: bool,
}

/// An alias the composer met, to resolve once the tree is built.
struct PendingAlias {
    /// Where the alias node is.
    at: Path,
    /// Where its anchor's node is, or `None` when the alias is inside it.
    target: Option<Path>,
}

/// decode.go's `parser`.
struct Composer<'a> {
    parser: Parser<'a>,
    /// The peeked event; `EventType::No` when there is none.
    event: Event,
    anchors: HashMap<String, Anchor>,
    next_anchor: usize,
    /// The path of the node being built.
    path: Path,
    aliases: Vec<PendingAlias>,
}

impl Composer<'_> {
    /// `parser.init`.
    fn init(&mut self) -> Result<(), YamlError> {
        self.expect(EventType::StreamStart)
    }

    /// `parser.expect`: consumes an event of type `want`.
    fn expect(&mut self, want: EventType) -> Result<(), YamlError> {
        if self.event.typ == EventType::No {
            self.event = self.parser.parse()?;
        }
        if self.event.typ == EventType::StreamEnd {
            return Err(YamlError::fail(
                "attempted to go past the end of stream; corrupted value?",
            ));
        }
        if self.event.typ != want {
            return Err(YamlError::fail(format!(
                "expected {want} event but got {}",
                self.event.typ
            )));
        }
        self.event = Event::default();
        Ok(())
    }

    /// `parser.peek`: the type of the next event, which stays queued.
    fn peek(&mut self) -> Result<EventType, YamlError> {
        if self.event.typ != EventType::No {
            return Ok(self.event.typ);
        }
        self.event = self.parser.parse()?;
        Ok(self.event.typ)
    }

    /// `parser.anchor`: notes where `anchor`'s node is.
    fn anchor(&mut self, node: &mut Node, anchor: &[u8]) -> Option<(String, usize)> {
        if anchor.is_empty() {
            return None;
        }
        node.anchor = text(anchor);
        let id = self.next_anchor;
        self.next_anchor = self.next_anchor.saturating_add(1);
        self.anchors.insert(
            node.anchor.clone(),
            Anchor {
                id,
                path: self.path.clone(),
                done: false,
            },
        );
        Some((node.anchor.clone(), id))
    }

    /// Marks an anchor's node as ended, unless a later definition took the
    /// name.
    fn anchor_done(&mut self, anchor: Option<(String, usize)>) {
        if let Some((name, id)) = anchor
            && let Some(entry) = self.anchors.get_mut(&name)
            && entry.id == id
        {
            entry.done = true;
        }
    }

    /// `parser.node`: a node of `kind` from the peeked event.
    fn node(&self, kind: Kind, default_tag: &str, tag: &[u8], value: &[u8]) -> Node {
        let mut style = Style::NONE;
        let tag = if !tag.is_empty() && tag != b"!" {
            style = Style::TAGGED;
            super::short_tag(&text(tag))
        } else if !default_tag.is_empty() {
            default_tag.to_owned()
        } else if kind == Kind::Scalar {
            super::resolve_tag(&text(value))
        } else {
            text(tag)
        };
        Node {
            kind,
            tag,
            value: text(value),
            style,
            line: self.event.start_mark.line.saturating_add(1),
            column: self.event.start_mark.column.saturating_add(1),
            head_comment: text(&self.event.head_comment),
            line_comment: text(&self.event.line_comment),
            foot_comment: text(&self.event.foot_comment),
            ..Node::default()
        }
    }

    /// `parser.parse`: the next node, or `None` at the end of the stream.
    ///
    /// yaml.v3 builds the tree by recursion; this keeps the open
    /// collections on a stack instead, so deep documents can't exhaust
    /// the thread's stack. The events are read in the same order.
    fn parse(&mut self) -> Result<Option<Node>, YamlError> {
        let mut stack: Vec<Open> = Vec::new();
        loop {
            let ends = match stack.last() {
                Some(open) => self.ends(open)?,
                None => false,
            };
            let node = if ends {
                let Some(open) = stack.pop() else {
                    return Err(invalid_state());
                };
                self.close(open)?
            } else {
                if let Some(parent) = stack.last() {
                    // parser.parseChild.
                    self.path.push(parent.node.content.len());
                }
                match self.start(stack.len())? {
                    Started::Node(node) => node,
                    Started::Open(open) => {
                        stack.push(open);
                        continue;
                    }
                    // parse only finds the end of the stream at the top.
                    Started::End if stack.is_empty() => return Ok(None),
                    Started::End => {
                        return Err(YamlError::fail(
                            "attempted to go past the end of stream; corrupted value?",
                        ));
                    }
                }
            };
            let Some(parent) = stack.last_mut() else {
                return Ok(Some(node));
            };
            self.path.pop();
            self.attach(parent, node)?;
        }
    }

    /// The start of `parser.parse`: a scalar or alias node, or a
    /// collection or document whose start event has been read.
    fn start(&mut self, depth: usize) -> Result<Started, YamlError> {
        let typ = self.peek()?;
        if depth > MAX_DEPTH && typ != EventType::StreamEnd {
            return Err(YamlError::fail(format!(
                "line {}: exceeded max depth of {MAX_DEPTH}",
                self.event.start_mark.line.saturating_add(1)
            )));
        }
        Ok(match typ {
            EventType::Scalar => Started::Node(self.scalar()?),
            EventType::Alias => Started::Node(self.alias()?),
            EventType::MappingStart => Started::Open(self.mapping_start()?),
            EventType::SequenceStart => Started::Open(self.sequence_start()?),
            EventType::DocumentStart => Started::Open(self.document_start()?),
            // Happens when attempting to decode an empty buffer.
            EventType::StreamEnd => Started::End,
            EventType::TailComment => {
                return Err(YamlError::fail(
                    "internal error: unexpected tail comment event",
                ));
            }
            other => {
                return Err(YamlError::fail(format!(
                    "internal error: attempted to parse unknown event: {other}"
                )));
            }
        })
    }

    /// Whether `open` ends at the next event: a sequence or mapping at its
    /// end event (a mapping only between pairs), a document once it has
    /// its node.
    fn ends(&mut self, open: &Open) -> Result<bool, YamlError> {
        Ok(match open.node.kind {
            Kind::Sequence => self.peek()? == EventType::SequenceEnd,
            Kind::Mapping => {
                open.node.content.len().is_multiple_of(2) && self.peek()? == EventType::MappingEnd
            }
            _ => !open.node.content.is_empty(),
        })
    }

    /// Adds a finished node to its parent, with yaml.v3's moves of foot
    /// comments between a mapping's keys and values.
    fn attach(&mut self, parent: &mut Open, child: Node) -> Result<(), YamlError> {
        let node = &mut parent.node;
        node.content.push(child);
        if node.kind != Kind::Mapping {
            return Ok(());
        }
        let len = node.content.len();
        if len % 2 == 1 {
            // A key.
            if !node.style.has(Style::FLOW) && len > 2 {
                // Must be a foot comment for the prior value when being
                // dedented.
                let foot = node
                    .content
                    .last_mut()
                    .map(|key| std::mem::take(&mut key.foot_comment))
                    .unwrap_or_default();
                if !foot.is_empty()
                    && let Some(prior) = node.content.get_mut(len - 3)
                {
                    prior.foot_comment = foot;
                }
            }
            return Ok(());
        }
        // A value.
        if let [.., key, value] = node.content.as_mut_slice()
            && key.foot_comment.is_empty()
            && !value.foot_comment.is_empty()
        {
            key.foot_comment = std::mem::take(&mut value.foot_comment);
        }
        if self.peek()? == EventType::TailComment {
            if let Some(key) = len.checked_sub(2).and_then(|at| node.content.get_mut(at))
                && key.foot_comment.is_empty()
            {
                key.foot_comment = text(&self.event.foot_comment);
            }
            self.expect(EventType::TailComment)?;
        }
        Ok(())
    }

    /// The end of `parser.sequence`, `parser.mapping` or `parser.document`.
    fn close(&mut self, open: Open) -> Result<Node, YamlError> {
        let Open { mut node, anchor } = open;
        match node.kind {
            Kind::Sequence => {
                node.line_comment = text(&self.event.line_comment);
                node.foot_comment = text(&self.event.foot_comment);
                self.expect(EventType::SequenceEnd)?;
            }
            Kind::Mapping => {
                node.line_comment = text(&self.event.line_comment);
                node.foot_comment = text(&self.event.foot_comment);
                if !node.style.has(Style::FLOW)
                    && !node.foot_comment.is_empty()
                    && node.content.len() > 1
                {
                    let foot = std::mem::take(&mut node.foot_comment);
                    let at = node.content.len() - 2;
                    if let Some(key) = node.content.get_mut(at) {
                        key.foot_comment = foot;
                    }
                }
                self.expect(EventType::MappingEnd)?;
            }
            _ => {
                if self.peek()? == EventType::DocumentEnd {
                    node.foot_comment = text(&self.event.foot_comment);
                }
                self.expect(EventType::DocumentEnd)?;
            }
        }
        self.anchor_done(anchor);
        Ok(node)
    }

    /// The start of `parser.document`.
    fn document_start(&mut self) -> Result<Open, YamlError> {
        let node = self.node(Kind::Document, "", b"", b"");
        self.expect(EventType::DocumentStart)?;
        Ok(Open { node, anchor: None })
    }

    /// `parser.alias`.
    fn alias(&mut self) -> Result<Node, YamlError> {
        let anchor = std::mem::take(&mut self.event.anchor);
        let node = self.node(Kind::Alias, "", b"", &anchor);
        let Some(entry) = self.anchors.get(&node.value) else {
            return Err(YamlError::fail(format!(
                "unknown anchor '{}' referenced",
                node.value
            )));
        };
        self.aliases.push(PendingAlias {
            at: self.path.clone(),
            target: entry.done.then(|| entry.path.clone()),
        });
        self.expect(EventType::Alias)?;
        Ok(node)
    }

    /// `parser.scalar`.
    fn scalar(&mut self) -> Result<Node, YamlError> {
        let node_style = match self.event.scalar_style {
            ScalarStyle::DoubleQuoted => Style::DOUBLE_QUOTED,
            ScalarStyle::SingleQuoted => Style::SINGLE_QUOTED,
            ScalarStyle::Literal => Style::LITERAL,
            ScalarStyle::Folded => Style::FOLDED,
            ScalarStyle::Any | ScalarStyle::Plain => Style::NONE,
        };
        let value = std::mem::take(&mut self.event.value);
        let tag = std::mem::take(&mut self.event.tag);
        let default_tag = if node_style.is_empty() {
            if value == b"<<" { MERGE_TAG } else { "" }
        } else {
            STR_TAG
        };
        let mut node = self.node(Kind::Scalar, default_tag, &tag, &value);
        node.style |= node_style;
        let anchor = std::mem::take(&mut self.event.anchor);
        let anchor = self.anchor(&mut node, &anchor);
        self.anchor_done(anchor);
        self.expect(EventType::Scalar)?;
        Ok(node)
    }

    /// The start of `parser.sequence`.
    fn sequence_start(&mut self) -> Result<Open, YamlError> {
        let tag = std::mem::take(&mut self.event.tag);
        let mut node = self.node(Kind::Sequence, SEQ_TAG, &tag, b"");
        if self.event.collection_style == CollectionStyle::Flow {
            node.style |= Style::FLOW;
        }
        let anchor = std::mem::take(&mut self.event.anchor);
        let anchor = self.anchor(&mut node, &anchor);
        self.expect(EventType::SequenceStart)?;
        Ok(Open { node, anchor })
    }

    /// The start of `parser.mapping`.
    fn mapping_start(&mut self) -> Result<Open, YamlError> {
        let tag = std::mem::take(&mut self.event.tag);
        let mut node = self.node(Kind::Mapping, MAP_TAG, &tag, b"");
        if self.event.collection_style == CollectionStyle::Flow {
            node.style |= Style::FLOW;
        }
        let anchor = std::mem::take(&mut self.event.anchor);
        let anchor = self.anchor(&mut node, &anchor);
        self.expect(EventType::MappingStart)?;
        Ok(Open { node, anchor })
    }
}

/// A sequence, mapping or document the composer is building.
struct Open {
    node: Node,
    /// The node's anchor and its definition's id.
    anchor: Option<(String, usize)>,
}

/// What [`Composer::start`] read.
enum Started {
    /// A finished scalar or alias.
    Node(Node),
    /// A collection or document, now open.
    Open(Open),
    /// The end of the stream.
    End,
}

/// The error for a composer state that can't happen.
fn invalid_state() -> YamlError {
    YamlError::fail("internal error: invalid composer state")
}

/// Text from the parser, which only produces UTF-8.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Gives each alias in `root` its target: a copy of its anchor's node in
/// the finished tree, as yaml.v3's pointer would show it.
fn resolve_aliases(root: &mut Node, aliases: &[PendingAlias]) -> Result<(), YamlError> {
    // An alias only names an anchor whose node ended before it, so every
    // alias inside that node is earlier and already resolved when a copy
    // is taken.
    let mut copies: HashMap<&[usize], Arc<Node>> = HashMap::new();
    let mut budget = MAX_ALIAS_COPIES;
    for alias in aliases {
        let target = match &alias.target {
            None => AliasTarget::Cycle,
            Some(path) => {
                let copy = match copies.get(path.as_slice()) {
                    Some(copy) => Arc::clone(copy),
                    None => {
                        let node = node_at(root, path).cloned().unwrap_or_default();
                        let size = count_nodes(&node);
                        budget = budget.checked_sub(size).ok_or_else(|| {
                            YamlError::fail("document contains excessive aliasing")
                        })?;
                        let copy = Arc::new(node);
                        copies.insert(path.as_slice(), Arc::clone(&copy));
                        copy
                    }
                };
                AliasTarget::Node(copy)
            }
        };
        if let Some(node) = node_at_mut(root, &alias.at) {
            node.alias = Some(target);
        }
    }
    Ok(())
}

/// The node at `path` below `root`.
fn node_at<'a>(root: &'a Node, path: &[usize]) -> Option<&'a Node> {
    path.iter()
        .try_fold(root, |node, &index| node.content.get(index))
}

/// The node at `path` below `root`, to change.
fn node_at_mut<'a>(root: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    path.iter()
        .try_fold(root, |node, &index| node.content.get_mut(index))
}

/// The number of nodes in a tree, not counting what aliases refer to.
fn count_nodes(node: &Node) -> usize {
    node.content
        .iter()
        .map(count_nodes)
        .fold(1, usize::saturating_add)
}

#[cfg(test)]
mod tests {
    //! [`unmarshal`] and the [`Encoder`](super::super::encode::Encoder)
    //! against yaml.v3's own answers, recorded with the Go probe (`node`,
    //! `roundtrip2` and `roundtrip` modes) in
    //! `src/config/testdata/yaml3/compose_nodes.json`. The inputs are the
    //! parser's recorded cases (`parser_events.json`, by name) and the
    //! config-shaped cases `cfg N`, whose input is stored with them.
    //!
    //! Each row has the case name `n`, yaml.v3's error `ne` or its tree
    //! `nd` in the canonical form of [`canon`] (or its length and FNV-1a
    //! digest when long), and the bytes `r2`/`r4` of `Encoder.Encode` of
    //! that tree with `SetIndent(2)` and the default indent, with the
    //! encoder's error in `r2e`/`r4e`. `go_panic` marks the inputs the
    //! probe couldn't report on (yaml.v3 accepts their 10000 levels; this
    //! port refuses them past 256).

    use std::collections::HashMap;

    use base64::Engine as _;
    use serde::Deserialize;
    use serde_json::Value;

    use super::super::encode::Encoder;
    use super::super::{Kind, Node};
    use super::unmarshal;

    /// The recorded trees and round trips.
    const NODES: &str = include_str!("../testdata/yaml3/compose_nodes.json");
    /// The parser's recorded cases, for their inputs.
    const EVENTS: &str = include_str!("../testdata/yaml3/parser_events.json");

    /// A row of `compose_nodes.json`.
    #[derive(Deserialize)]
    struct Row {
        n: String,
        #[serde(rename = "in")]
        input: Option<Input>,
        ne: Option<String>,
        nd: Option<Value>,
        r2: Option<Value>,
        r2e: Option<String>,
        r4: Option<Value>,
        r4e: Option<String>,
        go_panic: Option<String>,
    }

    /// A parser case, for its name and input.
    #[derive(Deserialize)]
    struct EventCase {
        n: String,
        #[serde(rename = "in")]
        input: Input,
    }

    /// A recorded input, as `parser_events.json` stores it.
    #[derive(Deserialize)]
    struct Input {
        s: Option<String>,
        b: Option<String>,
        r: Option<Vec<(String, usize)>>,
        rb: Option<Vec<(String, usize)>>,
    }

    impl Input {
        fn bytes(&self) -> Vec<u8> {
            let b64 = |s: &str| {
                base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .expect("base64 input")
            };
            if let Some(s) = &self.s {
                return s.as_bytes().to_vec();
            }
            if let Some(b) = &self.b {
                return b64(b);
            }
            if let Some(runs) = &self.r {
                return runs
                    .iter()
                    .flat_map(|(p, n)| p.as_bytes().repeat(*n))
                    .collect();
            }
            if let Some(runs) = &self.rb {
                return runs.iter().flat_map(|(p, n)| b64(p).repeat(*n)).collect();
            }
            panic!("input without data");
        }
    }

    /// FNV-1a 64, as the recorder computes it.
    fn fnv(data: &[u8]) -> String {
        let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
        for &byte in data {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01B3);
        }
        format!("{hash:016x}")
    }

    /// yaml.v3's number for a kind.
    fn kind_number(kind: Kind) -> u8 {
        match kind {
            Kind::Zero => 0,
            Kind::Document => 1,
            Kind::Sequence => 2,
            Kind::Mapping => 4,
            Kind::Scalar => 8,
            Kind::Alias => 16,
        }
    }

    /// The canonical form of a tree: one line per node, in document order,
    /// with its depth, kind, style, line, column, whether it is an alias,
    /// and its tag, value, anchor and comments, each as `length:text`.
    fn canon(node: &Node, depth: usize, out: &mut String) {
        let s = |text: &str| format!("{}:{text}", text.len());
        let fields = [
            depth.to_string(),
            kind_number(node.kind).to_string(),
            node.style.0.to_string(),
            node.line.to_string(),
            node.column.to_string(),
            if node.alias.is_some() { "1" } else { "0" }.to_owned(),
            s(&node.tag),
            s(&node.value),
            s(&node.anchor),
            s(&node.head_comment),
            s(&node.line_comment),
            s(&node.foot_comment),
        ];
        out.push_str(&fields.join("|"));
        out.push('\n');
        for child in &node.content {
            canon(child, depth + 1, out);
        }
    }

    /// Whether `got` matches a recorded text or digest.
    fn matches(want: &Value, got: &str) -> bool {
        match want {
            Value::String(text) => text == got,
            Value::Object(digest) => {
                digest.get("len").and_then(Value::as_u64) == Some(got.len() as u64)
                    && digest.get("fnv").and_then(Value::as_str) == Some(&fnv(got.as_bytes()))
            }
            _ => false,
        }
    }

    /// `Encoder.Encode(&node)` and `Close` with an indent (0: the default).
    fn round_trip(node: &Node, indent: usize) -> Result<String, String> {
        let mut encoder = Encoder::new();
        if indent != 0 {
            encoder.set_indent(indent);
        }
        encoder
            .encode_node(node)
            .map_err(|error| format!("encode: {error}"))?;
        let bytes = encoder.close().map_err(|error| format!("close: {error}"))?;
        Ok(String::from_utf8(bytes).expect("UTF-8 output"))
    }

    /// Compares one recorded round trip; returns a mismatch description.
    fn check_round_trip(
        node: &Node,
        indent: usize,
        want: Option<&Value>,
        want_error: Option<&String>,
    ) -> Option<String> {
        match (round_trip(node, indent), want_error) {
            (Err(got), Some(want)) if &got == want => None,
            (Err(got), _) => Some(format!(
                "indent {indent}: error {got:?}, want {want_error:?}"
            )),
            (Ok(_), Some(want)) => Some(format!("indent {indent}: no error, want {want:?}")),
            (Ok(got), None) => match want {
                Some(want) if matches(want, &got) => None,
                _ => Some(format!("indent {indent}: got {got:?}, want {want:?}")),
            },
        }
    }

    // Not upstream's: yaml.v3's Unmarshal into a yaml.Node and its
    // Encoder, recorded from the Go probe over the parser's 1,398 inputs
    // (yaml.v3's own test literals among them) and config-shaped inputs.
    #[test]
    fn trees_and_round_trips_match_yaml_v3() {
        let inputs: HashMap<String, Input> = serde_json::from_str::<Vec<EventCase>>(EVENTS)
            .expect("parser_events.json")
            .into_iter()
            .map(|case| (case.n, case.input))
            .collect();
        let rows: Vec<Row> = serde_json::from_str(NODES).expect("compose_nodes.json");
        assert!(rows.len() > 1400);
        let mut failures = Vec::new();
        let (mut trees, mut errors, mut trips) = (0, 0, 0);
        for row in &rows {
            let input = row
                .input
                .as_ref()
                .or_else(|| inputs.get(&row.n))
                .expect("input")
                .bytes();
            let result = unmarshal(&input);
            if row.go_panic.is_some() {
                // yaml.v3 nests 10000 levels; this port refuses past 256.
                match &result {
                    Err(error) if error.message().contains("exceeded max depth of 256") => {}
                    other => failures.push(format!("{}: {other:?}, want the depth error", row.n)),
                }
                continue;
            }
            match (&result, &row.ne) {
                // yaml.v3's scanner refuses 10000 levels; this port 256.
                (Err(got), Some(want))
                    if want == "yaml: exceeded max depth of 10000"
                        && got.message().ends_with("exceeded max depth of 256") =>
                {
                    errors += 1;
                }
                (Err(got), Some(want)) => {
                    if got.message() != want {
                        failures.push(format!(
                            "{}: error {:?}, want {want:?}",
                            row.n,
                            got.message()
                        ));
                    }
                    errors += 1;
                }
                (Err(got), None) => {
                    failures.push(format!("{}: error {:?}, want a tree", row.n, got.message()));
                }
                (Ok(_), Some(want)) => {
                    failures.push(format!("{}: a tree, want error {want:?}", row.n));
                }
                (Ok(node), None) => {
                    let mut text = String::new();
                    canon(node, 0, &mut text);
                    if !row.nd.as_ref().is_some_and(|want| matches(want, &text)) {
                        failures.push(format!("{}: tree\n{text}want\n{:?}", row.n, row.nd));
                        continue;
                    }
                    trees += 1;
                    if row.n == "tag escape surrogate" {
                        // The tag's %-escapes decode to bytes that aren't
                        // UTF-8; yaml.v3 writes them back, while a tag
                        // here is text and holds U+FFFD instead.
                        continue;
                    }
                    for (indent, want, want_error) in [
                        (2, row.r2.as_ref(), row.r2e.as_ref()),
                        (0, row.r4.as_ref(), row.r4e.as_ref()),
                    ] {
                        match check_round_trip(node, indent, want, want_error) {
                            Some(failure) => failures.push(format!("{}: {failure}", row.n)),
                            None => trips += 1,
                        }
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} mismatches:\n{}",
            failures.len(),
            failures
                .iter()
                .take(12)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(
            trees > 1000 && errors > 300 && trips > 2000,
            "{trees} {errors} {trips}"
        );
    }
}
