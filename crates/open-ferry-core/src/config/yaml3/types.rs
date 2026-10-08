// Ported from gopkg.in/yaml.v3 v3.0.1 yamlh.go (yaml_mark_t, yaml_encoding_t,
// yaml_break_t, yaml_error_type_t, the style, token and event types and the
// tag constants) and apic.go (the yaml_*_event_initialize functions) (MIT,
// from libyaml), the YAML library CLIProxyAPI v8.0.20 (MIT) reads and writes
// its config with.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/go-yaml/yaml
//
// Copyright (c) 2006-2010 Kirill Simonov
// Copyright (c) 2006-2011 Kirill Simonov
// Copyright (c) 2011-2019 Canonical Ltd
// Licensed under the MIT License; see licenses/go-yaml-LICENSE.

//! The values the scanner, parser and emitter pass around: marks, styles,
//! tokens and events.
//!
//! Deviations from upstream:
//! - An event's style is two fields, [`Event::scalar_style`] and
//!   [`Event::collection_style`], where yaml.v3 has one `style` that each
//!   event type reads as its own kind of style.
//! - Errors are values ([`ParserError`], [`EmitterError`]) rather than
//!   fields set on the parser or emitter.

use std::fmt;

/// A position in the input (`yaml_mark_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Mark {
    /// The position index, in characters.
    pub(crate) index: usize,
    /// The position line, from 0.
    pub(crate) line: usize,
    /// The position column, from 0.
    pub(crate) column: usize,
}

/// A stream encoding (`yaml_encoding_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// Let the parser choose the encoding.
    #[default]
    Any,
    /// UTF-8.
    Utf8,
    /// UTF-16 little-endian, with a byte order mark.
    Utf16Le,
    /// UTF-16 big-endian, with a byte order mark.
    Utf16Be,
}

/// A line break style (`yaml_break_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Break {
    /// Let the emitter choose.
    #[default]
    Any,
    /// CR (`\r`).
    #[cfg_attr(not(test), allow(dead_code))] // yaml.v3 only writes LN.
    Cr,
    /// LN (`\n`).
    Ln,
    /// CR LN (`\r\n`).
    #[cfg_attr(not(test), allow(dead_code))] // yaml.v3 only writes LN.
    CrLn,
}

/// The kind of an error (`yaml_error_type_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ErrorType {
    /// No error.
    #[default]
    No,
    /// Memory could not be allocated.
    #[allow(dead_code)] // libyaml's; never raised here, as in yaml.v3.
    Memory,
    /// The input could not be read or decoded.
    Reader,
    /// The input could not be scanned.
    Scanner,
    /// The input could not be parsed.
    Parser,
    /// A document could not be composed.
    #[allow(dead_code)] // libyaml's; never raised here, as in yaml.v3.
    Composer,
    /// The output could not be written.
    #[allow(dead_code)] // libyaml's; never raised here, as in yaml.v3.
    Writer,
    /// The stream could not be emitted.
    Emitter,
}

/// A scalar style (`yaml_scalar_style_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ScalarStyle {
    /// Let the emitter choose.
    #[default]
    Any,
    /// Plain.
    Plain,
    /// Single-quoted.
    SingleQuoted,
    /// Double-quoted.
    DoubleQuoted,
    /// Literal (`|`).
    Literal,
    /// Folded (`>`).
    Folded,
}

/// A sequence or mapping style (`yaml_sequence_style_t` and
/// `yaml_mapping_style_t`, which have the same values).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CollectionStyle {
    /// Let the emitter choose.
    #[default]
    Any,
    /// Block style.
    Block,
    /// Flow style (`[...]`, `{...}`).
    Flow,
}

/// A version directive (`yaml_version_directive_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VersionDirective {
    /// The major version number.
    pub(crate) major: i8,
    /// The minor version number.
    pub(crate) minor: i8,
}

/// A tag directive (`yaml_tag_directive_t`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TagDirective {
    /// The tag handle.
    pub(crate) handle: Vec<u8>,
    /// The tag prefix.
    pub(crate) prefix: Vec<u8>,
}

/// A token type (`yaml_token_type_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TokenType {
    /// An empty token.
    #[default]
    No,
    /// A STREAM-START token.
    StreamStart,
    /// A STREAM-END token.
    StreamEnd,
    /// A VERSION-DIRECTIVE token.
    VersionDirective,
    /// A TAG-DIRECTIVE token.
    TagDirective,
    /// A DOCUMENT-START token.
    DocumentStart,
    /// A DOCUMENT-END token.
    DocumentEnd,
    /// A BLOCK-SEQUENCE-START token.
    BlockSequenceStart,
    /// A BLOCK-MAPPING-START token.
    BlockMappingStart,
    /// A BLOCK-END token.
    BlockEnd,
    /// A FLOW-SEQUENCE-START token.
    FlowSequenceStart,
    /// A FLOW-SEQUENCE-END token.
    FlowSequenceEnd,
    /// A FLOW-MAPPING-START token.
    FlowMappingStart,
    /// A FLOW-MAPPING-END token.
    FlowMappingEnd,
    /// A BLOCK-ENTRY token.
    BlockEntry,
    /// A FLOW-ENTRY token.
    FlowEntry,
    /// A KEY token.
    Key,
    /// A VALUE token.
    Value,
    /// An ALIAS token.
    Alias,
    /// An ANCHOR token.
    Anchor,
    /// A TAG token.
    Tag,
    /// A SCALAR token.
    Scalar,
}

/// A token (`yaml_token_t`).
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Token {
    /// The token type.
    pub(crate) typ: TokenType,
    /// The beginning of the token.
    pub(crate) start_mark: Mark,
    /// The end of the token.
    pub(crate) end_mark: Mark,
    /// The stream encoding (for STREAM-START).
    pub(crate) encoding: Encoding,
    /// The alias, anchor or scalar value, or the tag directive handle.
    pub(crate) value: Vec<u8>,
    /// The tag suffix (for TAG).
    pub(crate) suffix: Vec<u8>,
    /// The tag directive prefix (for TAG-DIRECTIVE).
    pub(crate) prefix: Vec<u8>,
    /// The scalar style (for SCALAR).
    pub(crate) style: ScalarStyle,
    /// The major version number (for VERSION-DIRECTIVE).
    pub(crate) major: i8,
    /// The minor version number (for VERSION-DIRECTIVE).
    pub(crate) minor: i8,
}

impl fmt::Debug for Token {
    // A token's text may be a secret; show only its type and position.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("typ", &self.typ)
            .field("start_mark", &self.start_mark)
            .field("end_mark", &self.end_mark)
            .finish_non_exhaustive()
    }
}

/// An event type (`yaml_event_type_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum EventType {
    /// An empty event.
    #[default]
    No,
    /// A STREAM-START event.
    StreamStart,
    /// A STREAM-END event.
    StreamEnd,
    /// A DOCUMENT-START event.
    DocumentStart,
    /// A DOCUMENT-END event.
    DocumentEnd,
    /// An ALIAS event.
    Alias,
    /// A SCALAR event.
    Scalar,
    /// A SEQUENCE-START event.
    SequenceStart,
    /// A SEQUENCE-END event.
    SequenceEnd,
    /// A MAPPING-START event.
    MappingStart,
    /// A MAPPING-END event.
    MappingEnd,
    /// A TAIL-COMMENT event.
    TailComment,
}

impl EventType {
    /// The event's name in yaml.v3's messages (`eventStrings`).
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::No => "none",
            Self::StreamStart => "stream start",
            Self::StreamEnd => "stream end",
            Self::DocumentStart => "document start",
            Self::DocumentEnd => "document end",
            Self::Alias => "alias",
            Self::Scalar => "scalar",
            Self::SequenceStart => "sequence start",
            Self::SequenceEnd => "sequence end",
            Self::MappingStart => "mapping start",
            Self::MappingEnd => "mapping end",
            Self::TailComment => "tail comment",
        }
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An event (`yaml_event_t`).
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Event {
    /// The event type.
    pub(crate) typ: EventType,
    /// The beginning of the event.
    pub(crate) start_mark: Mark,
    /// The end of the event.
    pub(crate) end_mark: Mark,
    /// The document encoding (for STREAM-START).
    pub(crate) encoding: Encoding,
    /// The version directive (for DOCUMENT-START).
    pub(crate) version_directive: Option<VersionDirective>,
    /// The tag directives (for DOCUMENT-START).
    pub(crate) tag_directives: Vec<TagDirective>,
    /// The comments before the node.
    pub(crate) head_comment: Vec<u8>,
    /// The comment on the node's line.
    pub(crate) line_comment: Vec<u8>,
    /// The comments after the node.
    pub(crate) foot_comment: Vec<u8>,
    /// The comments at the end of a block (for TAIL-COMMENT).
    pub(crate) tail_comment: Vec<u8>,
    /// The anchor (for SCALAR, SEQUENCE-START, MAPPING-START and ALIAS).
    pub(crate) anchor: Vec<u8>,
    /// The tag (for SCALAR, SEQUENCE-START and MAPPING-START).
    pub(crate) tag: Vec<u8>,
    /// The scalar value (for SCALAR).
    pub(crate) value: Vec<u8>,
    /// Whether the document start or end indicator is implicit, or the tag
    /// is optional (for SCALAR this is `plain_implicit`).
    pub(crate) implicit: bool,
    /// Whether the tag is optional for any non-plain style (for SCALAR).
    pub(crate) quoted_implicit: bool,
    /// The style (for SCALAR).
    pub(crate) scalar_style: ScalarStyle,
    /// The style (for SEQUENCE-START and MAPPING-START).
    pub(crate) collection_style: CollectionStyle,
}

impl fmt::Debug for Event {
    // An event's text may be a secret; show only its type and position.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Event")
            .field("typ", &self.typ)
            .field("start_mark", &self.start_mark)
            .field("end_mark", &self.end_mark)
            .finish_non_exhaustive()
    }
}

impl Event {
    /// `yaml_stream_start_event_initialize`.
    pub(crate) fn stream_start(encoding: Encoding) -> Self {
        Self {
            typ: EventType::StreamStart,
            encoding,
            ..Self::default()
        }
    }

    /// `yaml_stream_end_event_initialize`.
    pub(crate) fn stream_end() -> Self {
        Self {
            typ: EventType::StreamEnd,
            ..Self::default()
        }
    }

    /// `yaml_document_start_event_initialize`.
    pub(crate) fn document_start(
        version_directive: Option<VersionDirective>,
        tag_directives: Vec<TagDirective>,
        implicit: bool,
    ) -> Self {
        Self {
            typ: EventType::DocumentStart,
            version_directive,
            tag_directives,
            implicit,
            ..Self::default()
        }
    }

    /// `yaml_document_end_event_initialize`.
    pub(crate) fn document_end(implicit: bool) -> Self {
        Self {
            typ: EventType::DocumentEnd,
            implicit,
            ..Self::default()
        }
    }

    /// `yaml_alias_event_initialize`.
    pub(crate) fn alias(anchor: Vec<u8>) -> Self {
        Self {
            typ: EventType::Alias,
            anchor,
            ..Self::default()
        }
    }

    /// `yaml_scalar_event_initialize`.
    pub(crate) fn scalar(
        anchor: Vec<u8>,
        tag: Vec<u8>,
        value: Vec<u8>,
        plain_implicit: bool,
        quoted_implicit: bool,
        style: ScalarStyle,
    ) -> Self {
        Self {
            typ: EventType::Scalar,
            anchor,
            tag,
            value,
            implicit: plain_implicit,
            quoted_implicit,
            scalar_style: style,
            ..Self::default()
        }
    }

    /// `yaml_sequence_start_event_initialize`.
    pub(crate) fn sequence_start(
        anchor: Vec<u8>,
        tag: Vec<u8>,
        implicit: bool,
        style: CollectionStyle,
    ) -> Self {
        Self {
            typ: EventType::SequenceStart,
            anchor,
            tag,
            implicit,
            collection_style: style,
            ..Self::default()
        }
    }

    /// `yaml_sequence_end_event_initialize`.
    pub(crate) fn sequence_end() -> Self {
        Self {
            typ: EventType::SequenceEnd,
            ..Self::default()
        }
    }

    /// `yaml_mapping_start_event_initialize`.
    pub(crate) fn mapping_start(
        anchor: Vec<u8>,
        tag: Vec<u8>,
        implicit: bool,
        style: CollectionStyle,
    ) -> Self {
        Self {
            typ: EventType::MappingStart,
            anchor,
            tag,
            implicit,
            collection_style: style,
            ..Self::default()
        }
    }

    /// `yaml_mapping_end_event_initialize`.
    pub(crate) fn mapping_end() -> Self {
        Self {
            typ: EventType::MappingEnd,
            ..Self::default()
        }
    }
}

/// A parse failure: the parser's `error`, `problem`, `context` and marks
/// fields at the point it stopped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ParserError {
    /// The kind of error.
    pub(crate) kind: ErrorType,
    /// The error description.
    pub(crate) problem: String,
    /// The byte offset of a reader error (`problem_offset`).
    pub(crate) problem_offset: usize,
    /// The bad value of a reader error, or -1 (`problem_value`).
    pub(crate) problem_value: i64,
    /// The position of the problem.
    pub(crate) problem_mark: Mark,
    /// The error context.
    pub(crate) context: String,
    /// The position of the context.
    pub(crate) context_mark: Mark,
}

impl ParserError {
    /// The message yaml.v3's `parser.fail` gives, without the `yaml: `
    /// prefix: `line N: <problem>`.
    pub(crate) fn message(&self) -> String {
        let mut line = 0;
        if self.context_mark.line != 0 {
            line = self.context_mark.line;
            // Scanner errors don't iterate line before returning error.
            if self.kind == ErrorType::Scanner {
                line += 1;
            }
        } else if self.problem_mark.line != 0 {
            line = self.problem_mark.line;
            if self.kind == ErrorType::Scanner {
                line += 1;
            }
        }
        let place = if line != 0 {
            format!("line {line}: ")
        } else {
            String::new()
        };
        let problem = if self.problem.is_empty() {
            "unknown problem parsing YAML content"
        } else {
            self.problem.as_str()
        };
        format!("{place}{problem}")
    }
}

/// An emit failure: the emitter's `error` and `problem` fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EmitterError {
    /// The kind of error.
    pub(crate) kind: ErrorType,
    /// The error description.
    pub(crate) problem: String,
}

impl EmitterError {
    /// An emitter error (`yaml_emitter_set_emitter_error`).
    pub(crate) fn emitter(problem: &str) -> Self {
        Self {
            kind: ErrorType::Emitter,
            problem: problem.to_owned(),
        }
    }

    /// The message yaml.v3's `encoder.must` gives, without the `yaml: `
    /// prefix.
    pub(crate) fn message(&self) -> String {
        if self.problem.is_empty() {
            "unknown problem generating YAML content".to_owned()
        } else {
            self.problem.clone()
        }
    }
}
