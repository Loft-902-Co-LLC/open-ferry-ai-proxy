// Ported from gopkg.in/yaml.v3 v3.0.1 yaml.go (Kind, Style, Node,
// Node.IsZero, Node.LongTag, Node.ShortTag, Node.indicatedString,
// Node.SetString) and resolve.go (shortTag, longTag) (Apache-2.0), the YAML
// library CLIProxyAPI v8.0.15 (MIT) reads and writes its config with.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/go-yaml/yaml
//
// Copyright (c) 2011-2019 Canonical Ltd
// Licensed under the Apache License, Version 2.0; see licenses/go-yaml-LICENSE
// and licenses/go-yaml-NOTICE.

//! yaml.v3's scanner, parser, node tree and emitter, for writing the config
//! file back as upstream does.
//!
//! Upstream rewrites its config with yaml.v3: it reads the file into a
//! `yaml.Node` tree that keeps comments, styles and key order, merges the
//! new settings into it, and writes the tree out with yaml.v3's emitter.
//! The bytes that come out depend on every detail of that library, so this
//! module ports it: the reader, scanner and parser (`readerc.go`,
//! `scannerc.go`, `parserc.go`) turn bytes into events, [`compose`] builds a
//! [`Node`] tree from them as `yaml.Unmarshal` into a `yaml.Node` does,
//! [`encode`] turns a tree or a Go-shaped value back into events as
//! `yaml.Encoder` does, and the emitter (`emitterc.go`, `writerc.go`)
//! writes the events out.
//!
//! The config loader keeps its own reader ([`super::yaml`]); this one is
//! used to write.
//!
//! [`Node`]'s `Debug` shows a node's kind, line and size, never its text,
//! which may be a secret.
//!
//! Deviations from upstream:
//! - An alias holds a copy of its anchor's node as the finished tree has
//!   it, where yaml.v3's points to the node itself (see [`compose`]). The
//!   trees here are only edited after their aliases are expanded, so the
//!   two can't differ. An alias inside the node its anchor names, which
//!   yaml.v3 builds as a cycle, holds a marker instead, and expanding it is
//!   the error yaml.v3's decoder gives (`anchor 'a' value contains itself`).
//! - Nesting deeper than 256 levels is an error (`exceeded max depth of
//!   256`); yaml.v3 allows 10000.
//! - A node's tag, value, anchor and comments are text. A tag whose
//!   `%`-escapes decode to bytes that aren't UTF-8 holds U+FFFD for them
//!   and is written back that way, where yaml.v3 keeps the bytes.

pub(crate) mod chars;
pub(crate) mod compose;
pub(crate) mod emitter;
pub(crate) mod encode;
pub(crate) mod parser;
pub(crate) mod reader;
pub(crate) mod scanner;
pub(crate) mod types;

use std::fmt;
use std::sync::Arc;

/// The deepest nesting a document may have.
pub(crate) const MAX_DEPTH: usize = super::yaml::MAX_DEPTH;

/// The prefix of the long form of yaml.v3's standard tags.
pub(crate) const LONG_TAG_PREFIX: &str = "tag:yaml.org,2002:";

/// yaml.v3's short tags (`nullTag` and the rest).
pub(crate) const NULL_TAG: &str = "!!null";
/// `boolTag`.
pub(crate) const BOOL_TAG: &str = "!!bool";
/// `strTag`.
pub(crate) const STR_TAG: &str = "!!str";
/// `intTag`.
pub(crate) const INT_TAG: &str = "!!int";
/// `floatTag`.
pub(crate) const FLOAT_TAG: &str = "!!float";
/// `timestampTag`.
pub(crate) const TIMESTAMP_TAG: &str = "!!timestamp";
/// `seqTag`.
pub(crate) const SEQ_TAG: &str = "!!seq";
/// `mapTag`.
pub(crate) const MAP_TAG: &str = "!!map";
/// `mergeTag`.
pub(crate) const MERGE_TAG: &str = "!!merge";

/// yaml.v3's `shortTag`: `tag:yaml.org,2002:x` as `!!x`.
pub(crate) fn short_tag(tag: &str) -> String {
    match tag.strip_prefix(LONG_TAG_PREFIX) {
        Some(rest) => format!("!!{rest}"),
        None => tag.to_owned(),
    }
}

/// yaml.v3's `longTag`: `!!x` as `tag:yaml.org,2002:x`.
pub(crate) fn long_tag(tag: &str) -> String {
    match tag.strip_prefix("!!") {
        Some(rest) => format!("{LONG_TAG_PREFIX}{rest}"),
        None => tag.to_owned(),
    }
}

/// yaml.v3's `resolve("", value)` tag: the short tag a plain scalar
/// resolves to.
pub(crate) fn resolve_tag(value: &str) -> String {
    match super::yaml::resolve("", value) {
        Ok((tag, _)) => tag,
        Err(_) => STR_TAG.to_owned(),
    }
}

/// A node's kind (`yaml.Kind`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Kind {
    /// The zero kind of a node that was never set.
    #[default]
    Zero,
    /// A document (`DocumentNode`).
    Document,
    /// A sequence (`SequenceNode`).
    Sequence,
    /// A mapping (`MappingNode`).
    Mapping,
    /// A scalar (`ScalarNode`).
    Scalar,
    /// An alias (`AliasNode`).
    Alias,
}

/// A node's style flags (`yaml.Style`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Style(u8);

impl Style {
    /// No flags.
    pub(crate) const NONE: Self = Self(0);
    /// The tag was written out (`TaggedStyle`).
    pub(crate) const TAGGED: Self = Self(1);
    /// `DoubleQuotedStyle`.
    pub(crate) const DOUBLE_QUOTED: Self = Self(1 << 1);
    /// `SingleQuotedStyle`.
    pub(crate) const SINGLE_QUOTED: Self = Self(1 << 2);
    /// `LiteralStyle`.
    pub(crate) const LITERAL: Self = Self(1 << 3);
    /// `FoldedStyle`.
    pub(crate) const FOLDED: Self = Self(1 << 4);
    /// `FlowStyle`.
    pub(crate) const FLOW: Self = Self(1 << 5);

    /// Whether any of `flags` is set (`style&flags != 0`).
    pub(crate) fn has(self, flags: Self) -> bool {
        self.0 & flags.0 != 0
    }

    /// Whether no flag is set.
    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Style {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOrAssign for Style {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// What an alias refers to.
#[derive(Clone, PartialEq)]
pub(crate) enum AliasTarget {
    /// The anchor's node, as it was when it ended.
    Node(Arc<Node>),
    /// The anchor's node contains the alias.
    Cycle,
}

/// A YAML node with its comments and position (`yaml.Node`).
#[derive(Clone, Default, PartialEq)]
pub(crate) struct Node {
    /// The kind of node.
    pub(crate) kind: Kind,
    /// The style flags.
    pub(crate) style: Style,
    /// The tag, in short form for yaml.v3's standard tags (`!!str`).
    pub(crate) tag: String,
    /// The scalar value, or an alias's anchor name.
    pub(crate) value: String,
    /// The anchor the node defines.
    pub(crate) anchor: String,
    /// What an alias refers to.
    pub(crate) alias: Option<AliasTarget>,
    /// A document's root, a sequence's items, or a mapping's keys and
    /// values in turn.
    pub(crate) content: Vec<Node>,
    /// The comments before the node.
    pub(crate) head_comment: String,
    /// The comment after the node on its line.
    pub(crate) line_comment: String,
    /// The comments after the node.
    pub(crate) foot_comment: String,
    /// The line the node starts on, from 1, or 0 if unknown.
    pub(crate) line: usize,
    /// The column the node starts at, from 1, or 0 if unknown.
    pub(crate) column: usize,
}

impl fmt::Debug for Node {
    // A node's text may be a secret; show only its shape.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("kind", &self.kind)
            .field("style", &self.style)
            .field("line", &self.line)
            .field("column", &self.column)
            .field("content", &self.content)
            .finish_non_exhaustive()
    }
}

impl Node {
    /// A scalar node with a tag and value.
    pub(crate) fn scalar(tag: &str, value: &str) -> Self {
        Self {
            kind: Kind::Scalar,
            tag: tag.to_owned(),
            value: value.to_owned(),
            ..Self::default()
        }
    }

    /// An empty mapping node.
    pub(crate) fn mapping() -> Self {
        Self {
            kind: Kind::Mapping,
            tag: MAP_TAG.to_owned(),
            ..Self::default()
        }
    }

    /// An empty sequence node.
    pub(crate) fn sequence() -> Self {
        Self {
            kind: Kind::Sequence,
            tag: SEQ_TAG.to_owned(),
            ..Self::default()
        }
    }

    /// `Node.IsZero`: whether every field has its zero value.
    pub(crate) fn is_zero(&self) -> bool {
        self.kind == Kind::Zero
            && self.style.is_empty()
            && self.tag.is_empty()
            && self.value.is_empty()
            && self.anchor.is_empty()
            && self.alias.is_none()
            && self.content.is_empty()
            && self.head_comment.is_empty()
            && self.line_comment.is_empty()
            && self.foot_comment.is_empty()
            && self.line == 0
            && self.column == 0
    }

    /// `Node.LongTag`.
    #[cfg(test)]
    pub(crate) fn long_tag(&self) -> String {
        long_tag(&self.short_tag())
    }

    /// `Node.ShortTag`: the tag in short form, or the tag the node's kind
    /// or value implies when it has none.
    pub(crate) fn short_tag(&self) -> String {
        if self.indicated_string() {
            return STR_TAG.to_owned();
        }
        if self.tag.is_empty() || self.tag == "!" {
            return match self.kind {
                Kind::Mapping => MAP_TAG.to_owned(),
                Kind::Sequence => SEQ_TAG.to_owned(),
                Kind::Alias => match &self.alias {
                    Some(AliasTarget::Node(target)) => target.short_tag(),
                    _ => String::new(),
                },
                Kind::Scalar => resolve_tag(&self.value),
                Kind::Zero if self.is_zero() => NULL_TAG.to_owned(),
                _ => String::new(),
            };
        }
        short_tag(&self.tag)
    }

    /// `Node.indicatedString`: whether the node is a scalar its tag or
    /// quoting marks as a string.
    pub(crate) fn indicated_string(&self) -> bool {
        self.kind == Kind::Scalar
            && (short_tag(&self.tag) == STR_TAG
                || ((self.tag.is_empty() || self.tag == "!")
                    && self.style.has(
                        Style::SINGLE_QUOTED
                            | Style::DOUBLE_QUOTED
                            | Style::LITERAL
                            | Style::FOLDED,
                    )))
    }

    /// `Node.SetString`, for valid UTF-8 (a Rust string always is).
    #[cfg(test)]
    pub(crate) fn set_string(&mut self, value: &str) {
        self.kind = Kind::Scalar;
        value.clone_into(&mut self.value);
        STR_TAG.clone_into(&mut self.tag);
        if self.value.contains('\n') {
            self.style = Style::LITERAL;
        }
    }

    /// A mapping's keys and values in pairs; nothing for another kind.
    pub(crate) fn pairs(&self) -> impl Iterator<Item = (&Node, &Node)> {
        let content = if self.kind == Kind::Mapping {
            self.content.as_slice()
        } else {
            &[]
        };
        content
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[key, value]| (key, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: shortTag and longTag round-trip yaml.v3's standard
    // tags and leave others alone.
    #[test]
    fn tags_convert_between_forms() {
        assert_eq!(short_tag("tag:yaml.org,2002:str"), "!!str");
        assert_eq!(short_tag("tag:yaml.org,2002:custom"), "!!custom");
        assert_eq!(short_tag("!local"), "!local");
        assert_eq!(long_tag("!!int"), "tag:yaml.org,2002:int");
        assert_eq!(long_tag("!local"), "!local");
    }

    // Not upstream's: an untagged node's tag comes from its kind or value.
    #[test]
    fn short_tag_follows_kind_and_value() {
        let mut node = Node {
            kind: Kind::Scalar,
            value: "1".to_owned(),
            ..Node::default()
        };
        assert_eq!(node.short_tag(), INT_TAG);
        node.style = Style::DOUBLE_QUOTED;
        assert_eq!(node.short_tag(), STR_TAG);
        assert_eq!(Node::default().short_tag(), NULL_TAG);
        assert_eq!(Node::mapping().short_tag(), MAP_TAG);
        let alias = Node {
            kind: Kind::Alias,
            alias: Some(AliasTarget::Node(Arc::new(Node::sequence()))),
            ..Node::default()
        };
        assert_eq!(alias.short_tag(), SEQ_TAG);
        assert_eq!(alias.long_tag(), "tag:yaml.org,2002:seq");
    }

    // Not upstream's: SetString marks a multi-line string literal.
    #[test]
    fn set_string_marks_multi_line_literal() {
        let mut node = Node::default();
        node.set_string("a\nb");
        assert_eq!(node.kind, Kind::Scalar);
        assert_eq!(node.tag, STR_TAG);
        assert_eq!(node.style, Style::LITERAL);
    }
}
