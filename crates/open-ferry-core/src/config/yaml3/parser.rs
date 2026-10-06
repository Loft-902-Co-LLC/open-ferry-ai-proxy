// Ported from gopkg.in/yaml.v3 v3.0.1 parserc.go (peek_token,
// yaml_parser_unfold_comments, skip_token, yaml_parser_parse,
// yaml_parser_set_parser_error, yaml_parser_set_parser_error_context,
// yaml_parser_state_machine, the yaml_parser_parse_* functions,
// yaml_parser_set_event_comments, yaml_parser_split_stem_comment,
// yaml_parser_process_empty_scalar, yaml_parser_process_directives,
// yaml_parser_append_tag_directive), apic.go (yaml_parser_initialize,
// yaml_parser_set_input_string) and yamlh.go (yaml_parser_t,
// yaml_parser_state_t) (MIT, from libyaml), the YAML library CLIProxyAPI
// v8.0.15 (MIT) reads and writes its config with.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/go-yaml/yaml
//
// Copyright (c) 2006-2010 Kirill Simonov
// Copyright (c) 2006-2011 Kirill Simonov
// Copyright (c) 2011-2019 Canonical Ltd
// Licensed under the MIT License; see licenses/go-yaml-LICENSE.

//! The parser: it turns the scanner's tokens into events (STREAM-START,
//! DOCUMENT-START, MAPPING-START, SCALAR, ...), attaching the scanner's
//! comments to them as yaml.v3 does.
//!
//! The parser implements the following grammar:
//!
//! ```text
//! stream               ::= STREAM-START implicit_document? explicit_document* STREAM-END
//! implicit_document    ::= block_node DOCUMENT-END*
//! explicit_document    ::= DIRECTIVE* DOCUMENT-START block_node? DOCUMENT-END*
//! block_node_or_indentless_sequence    ::=
//!                          ALIAS
//!                          | properties (block_content | indentless_block_sequence)?
//!                          | block_content
//!                          | indentless_block_sequence
//! block_node           ::= ALIAS
//!                          | properties block_content?
//!                          | block_content
//! flow_node            ::= ALIAS
//!                          | properties flow_content?
//!                          | flow_content
//! properties           ::= TAG ANCHOR? | ANCHOR TAG?
//! block_content        ::= block_collection | flow_collection | SCALAR
//! flow_content         ::= flow_collection | SCALAR
//! block_collection     ::= block_sequence | block_mapping
//! flow_collection      ::= flow_sequence | flow_mapping
//! block_sequence       ::= BLOCK-SEQUENCE-START (BLOCK-ENTRY block_node?)* BLOCK-END
//! indentless_sequence  ::= (BLOCK-ENTRY block_node?)+
//! block_mapping        ::= BLOCK-MAPPING_START
//!                          ((KEY block_node_or_indentless_sequence?)?
//!                          (VALUE block_node_or_indentless_sequence?)?)*
//!                          BLOCK-END
//! flow_sequence        ::= FLOW-SEQUENCE-START
//!                          (flow_sequence_entry FLOW-ENTRY)*
//!                          flow_sequence_entry?
//!                          FLOW-SEQUENCE-END
//! flow_sequence_entry  ::= flow_node | KEY flow_node? (VALUE flow_node?)?
//! flow_mapping         ::= FLOW-MAPPING-START
//!                          (flow_mapping_entry FLOW-ENTRY)*
//!                          flow_mapping_entry?
//!                          FLOW-MAPPING-END
//! flow_mapping_entry   ::= flow_node | KEY flow_node? (VALUE flow_node?)?
//! ```
//!
//! [`Parser`] holds yaml_parser_t's reader, scanner and parser state; the
//! reader and the scanner are in `reader.rs` and `scanner.rs`.
//!
//! Deviations from upstream:
//! - Indexing is panic-free.
//! - [`Parser::parse`] reports an error when the state machine fails *or*
//!   leaves an error set, as decode.go's `parser.peek` checks it, and
//!   returns that error again on later calls; yaml_parser_parse returns
//!   true and an empty event after an error.
//! - Where yaml.v3 would pop an empty state or mark stack, or find no
//!   token after a successful fetch, this sets the parser error "invalid
//!   parser state" (yaml.v3's panic for an unknown state) instead of
//!   panicking. The parser pushes and pops in pairs, so none of these can
//!   happen.
//! - yaml_parser_parse_flow_mapping_key dereferences a nil token if the
//!   first peek fails; this returns false. That peek re-reads a token
//!   already fetched, so it can't fail.
//! - Token values are moved out of the queue when an event takes them;
//!   yaml.v3 shares them. The parser never reads a value twice.

use std::collections::HashMap;
use std::mem;

use super::chars::{INPUT_BUFFER_SIZE, INPUT_RAW_BUFFER_SIZE, at};
use super::scanner::{Comment, SimpleKey};
use super::types::{
    CollectionStyle, Encoding, ErrorType, Event, EventType, Mark, ParserError, ScalarStyle,
    TagDirective, Token, TokenType, VersionDirective,
};

/// yaml_parser_state_t: the states of the parser.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum State {
    /// Expect STREAM-START.
    #[default]
    StreamStart,
    /// Expect the beginning of an implicit document.
    ImplicitDocumentStart,
    /// Expect DOCUMENT-START.
    DocumentStart,
    /// Expect the content of a document.
    DocumentContent,
    /// Expect DOCUMENT-END.
    DocumentEnd,
    /// Expect a block node.
    BlockNode,
    /// Expect a block node or indentless sequence.
    #[allow(dead_code)] // libyaml's; never entered, as in yaml.v3.
    BlockNodeOrIndentlessSequence,
    /// Expect a flow node.
    #[allow(dead_code)] // libyaml's; never entered, as in yaml.v3.
    FlowNode,
    /// Expect the first entry of a block sequence.
    BlockSequenceFirstEntry,
    /// Expect an entry of a block sequence.
    BlockSequenceEntry,
    /// Expect an entry of an indentless sequence.
    IndentlessSequenceEntry,
    /// Expect the first key of a block mapping.
    BlockMappingFirstKey,
    /// Expect a block mapping key.
    BlockMappingKey,
    /// Expect a block mapping value.
    BlockMappingValue,
    /// Expect the first entry of a flow sequence.
    FlowSequenceFirstEntry,
    /// Expect an entry of a flow sequence.
    FlowSequenceEntry,
    /// Expect a key of an ordered mapping.
    FlowSequenceEntryMappingKey,
    /// Expect a value of an ordered mapping.
    FlowSequenceEntryMappingValue,
    /// Expect the and of an ordered mapping entry.
    FlowSequenceEntryMappingEnd,
    /// Expect the first key of a flow mapping.
    FlowMappingFirstKey,
    /// Expect a key of a flow mapping.
    FlowMappingKey,
    /// Expect a value of a flow mapping.
    FlowMappingValue,
    /// Expect an empty value of a flow mapping.
    FlowMappingEmptyValue,
    /// Expect nothing.
    End,
}

/// The parts of the head token peek_token's callers read, copied out of
/// the queue (the values are taken separately when an event needs them).
#[derive(Clone, Copy)]
struct TokenInfo {
    /// The token type.
    typ: TokenType,
    /// The beginning of the token.
    start_mark: Mark,
    /// The end of the token.
    end_mark: Mark,
    /// The stream encoding (for STREAM-START).
    encoding: Encoding,
    /// The scalar style (for SCALAR).
    style: ScalarStyle,
    /// The major version number (for VERSION-DIRECTIVE).
    major: i8,
    /// The minor version number (for VERSION-DIRECTIVE).
    minor: i8,
}

/// yaml_parser_t.
#[derive(Default)]
pub(crate) struct Parser<'a> {
    // Error handling
    /// Error type.
    pub(super) error: ErrorType,
    /// Error description.
    pub(super) problem: &'static str,
    /// The byte about which the problem occurred.
    pub(super) problem_offset: usize,
    /// The problematic value (-1 is none).
    pub(super) problem_value: i64,
    /// The problem position.
    pub(super) problem_mark: Mark,
    /// The error context.
    pub(super) context: &'static str,
    /// The context position.
    pub(super) context_mark: Mark,

    // Reader stuff
    /// String input data.
    pub(super) input: &'a [u8],
    /// The read position in the input.
    pub(super) input_pos: usize,
    /// EOF flag.
    pub(super) eof: bool,
    /// The working buffer.
    pub(super) buffer: Vec<u8>,
    /// The current position of the buffer.
    pub(super) buffer_pos: usize,
    /// The number of unread characters in the buffer.
    pub(super) unread: usize,
    /// The number of line breaks since last non-break/non-blank character.
    pub(super) newlines: usize,
    /// The raw buffer.
    pub(super) raw_buffer: Vec<u8>,
    /// The current position of the raw buffer.
    pub(super) raw_buffer_pos: usize,
    /// The input encoding.
    pub(super) encoding: Encoding,
    /// The offset of the current position (in bytes).
    pub(super) offset: usize,
    /// The mark of the current position.
    pub(super) mark: Mark,

    // Comments
    /// The current head comments.
    pub(super) head_comment: Vec<u8>,
    /// The current line comments.
    pub(super) line_comment: Vec<u8>,
    /// The current foot comments.
    pub(super) foot_comment: Vec<u8>,
    /// Foot comment that happens at the end of a block.
    pub(super) tail_comment: Vec<u8>,
    /// Comment in item preceding a nested structure (list inside list
    /// item, etc).
    pub(super) stem_comment: Vec<u8>,
    /// The folded comments for all parsed tokens.
    pub(super) comments: Vec<Comment>,
    /// The first comment not yet attached to an event.
    pub(super) comments_head: usize,

    // Scanner stuff
    /// Have we started to scan the input stream?
    pub(super) stream_start_produced: bool,
    /// Have we reached the end of the input stream?
    pub(super) stream_end_produced: bool,
    /// The number of unclosed '[' and '{' indicators.
    pub(super) flow_level: usize,
    /// The tokens queue.
    pub(super) tokens: Vec<Token>,
    /// The head of the tokens queue.
    pub(super) tokens_head: usize,
    /// The number of tokens fetched from the queue.
    pub(super) tokens_parsed: usize,
    /// Does the tokens queue contain a token ready for dequeueing.
    pub(super) token_available: bool,
    /// The current indentation level.
    pub(super) indent: i64,
    /// The indentation levels stack.
    pub(super) indents: Vec<i64>,
    /// May a simple key occur at the current position?
    pub(super) simple_key_allowed: bool,
    /// The stack of simple keys.
    pub(super) simple_keys: Vec<SimpleKey>,
    /// Possible simple key indexes indexed by token number.
    pub(super) simple_keys_by_tok: HashMap<usize, usize>,

    // Parser stuff
    /// The current parser state.
    pub(super) state: State,
    /// The parser states stack.
    pub(super) states: Vec<State>,
    /// The stack of marks.
    pub(super) marks: Vec<Mark>,
    /// The list of TAG directives.
    pub(super) tag_directives: Vec<TagDirective>,
}

/// yaml.v3's default_tag_directives, as (handle, prefix).
const DEFAULT_TAG_DIRECTIVES: [(&[u8], &[u8]); 2] = [(b"!", b"!"), (b"!!", b"tag:yaml.org,2002:")];

impl<'a> Parser<'a> {
    /// yaml_parser_initialize followed by yaml_parser_set_input_string:
    /// a parser reading `input`. Empty input stays empty (decode.go's
    /// newParser turns it into "\n"; that is the caller's business).
    pub(crate) fn new(input: &'a [u8]) -> Self {
        Self {
            raw_buffer: Vec::with_capacity(INPUT_RAW_BUFFER_SIZE),
            buffer: Vec::with_capacity(INPUT_BUFFER_SIZE),
            input,
            input_pos: 0,
            ..Self::default()
        }
    }
}

impl Parser<'_> {
    /// yaml_parser_parse: get the next event. After STREAM-END, or in the
    /// END state, it returns an event of type `EventType::No`; after an
    /// error it returns the same error again.
    pub(crate) fn parse(&mut self) -> Result<Event, ParserError> {
        // No events after the end of the stream or error.
        if self.error != ErrorType::No {
            return Err(self.error_fields());
        }
        if self.stream_end_produced || self.state == State::End {
            return Ok(Event::default());
        }

        // Generate the next event.
        let mut event = Event::default();
        let ok = self.state_machine(&mut event);
        if !ok || self.error != ErrorType::No {
            return Err(self.error_fields());
        }
        Ok(event)
    }

    /// The error fields as a [`ParserError`].
    pub(super) fn error_fields(&self) -> ParserError {
        ParserError {
            kind: self.error,
            problem: self.problem.to_owned(),
            problem_offset: self.problem_offset,
            problem_value: self.problem_value,
            problem_mark: self.problem_mark,
            context: self.context.to_owned(),
            context_mark: self.context_mark,
        }
    }

    /// peek_token: peek the next token in the token queue.
    fn peek_token(&mut self) -> Option<TokenInfo> {
        if self.token_available || self.fetch_more_tokens() {
            let Some(token) = self.tokens.get(self.tokens_head) else {
                // yaml.v3 would index out of range; see the module docs.
                self.set_parser_error("invalid parser state", self.mark);
                return None;
            };
            let info = TokenInfo {
                typ: token.typ,
                start_mark: token.start_mark,
                end_mark: token.end_mark,
                encoding: token.encoding,
                style: token.style,
                major: token.major,
                minor: token.minor,
            };
            self.unfold_comments(&info);
            return Some(info);
        }
        None
    }

    /// yaml_parser_unfold_comments walks through the comments queue and
    /// joins all comments behind the position of the provided token into
    /// the respective top-level comment slices in the parser.
    fn unfold_comments(&mut self, token: &TokenInfo) {
        while let Some(comment) = self.comments.get_mut(self.comments_head)
            && token.start_mark.index >= comment.token_mark.index
        {
            if !comment.head.is_empty() {
                if token.typ == TokenType::BlockEnd {
                    // No heads on ends, so keep comment.head for a follow
                    // up token.
                    break;
                }
                if !self.head_comment.is_empty() {
                    self.head_comment.push(b'\n');
                }
                self.head_comment.extend_from_slice(&comment.head);
            }
            if !comment.foot.is_empty() {
                if !self.foot_comment.is_empty() {
                    self.foot_comment.push(b'\n');
                }
                self.foot_comment.extend_from_slice(&comment.foot);
            }
            if !comment.line.is_empty() {
                if !self.line_comment.is_empty() {
                    self.line_comment.push(b'\n');
                }
                self.line_comment.extend_from_slice(&comment.line);
            }
            *comment = Comment::default();
            self.comments_head += 1;
        }
    }

    /// skip_token: remove the next token from the queue (must be called
    /// after peek_token).
    fn skip_token(&mut self) {
        self.token_available = false;
        self.tokens_parsed += 1;
        self.stream_end_produced = self
            .tokens
            .get(self.tokens_head)
            .is_some_and(|t| t.typ == TokenType::StreamEnd);
        self.tokens_head += 1;
    }

    /// The head token's value (token.value), moved out of the queue.
    fn take_token_value(&mut self) -> Vec<u8> {
        self.tokens
            .get_mut(self.tokens_head)
            .map(|t| mem::take(&mut t.value))
            .unwrap_or_default()
    }

    /// The head token's suffix (token.suffix), moved out of the queue.
    fn take_token_suffix(&mut self) -> Vec<u8> {
        self.tokens
            .get_mut(self.tokens_head)
            .map(|t| mem::take(&mut t.suffix))
            .unwrap_or_default()
    }

    /// The head token's prefix (token.prefix), moved out of the queue.
    fn take_token_prefix(&mut self) -> Vec<u8> {
        self.tokens
            .get_mut(self.tokens_head)
            .map(|t| mem::take(&mut t.prefix))
            .unwrap_or_default()
    }

    /// yaml_parser_set_parser_error: set the parser error and return
    /// false.
    fn set_parser_error(&mut self, problem: &'static str, problem_mark: Mark) -> bool {
        self.error = ErrorType::Parser;
        self.problem = problem;
        self.problem_mark = problem_mark;
        false
    }

    /// yaml_parser_set_parser_error_context: set the parser error with a
    /// context and return false.
    fn set_parser_error_context(
        &mut self,
        context: &'static str,
        context_mark: Mark,
        problem: &'static str,
        problem_mark: Mark,
    ) -> bool {
        self.error = ErrorType::Parser;
        self.context = context;
        self.context_mark = context_mark;
        self.problem = problem;
        self.problem_mark = problem_mark;
        false
    }

    /// Pop the state stack into the current state
    /// (`parser.state = parser.states[len(parser.states)-1]`).
    fn pop_state(&mut self, mark: Mark) -> bool {
        match self.states.pop() {
            Some(state) => {
                self.state = state;
                true
            }
            // yaml.v3 would index out of range; see the module docs.
            None => self.set_parser_error("invalid parser state", mark),
        }
    }

    /// Pop the mark stack (`parser.marks = parser.marks[:len(parser.marks)-1]`).
    fn pop_mark(&mut self, mark: Mark) -> bool {
        if self.marks.pop().is_some() {
            return true;
        }
        // yaml.v3 would slice out of range; see the module docs.
        self.set_parser_error("invalid parser state", mark)
    }

    /// yaml_parser_state_machine: the state dispatcher.
    fn state_machine(&mut self, event: &mut Event) -> bool {
        match self.state {
            State::StreamStart => self.parse_stream_start(event),
            State::ImplicitDocumentStart => self.parse_document_start(event, true),
            State::DocumentStart => self.parse_document_start(event, false),
            State::DocumentContent => self.parse_document_content(event),
            State::DocumentEnd => self.parse_document_end(event),
            State::BlockNode => self.parse_node(event, true, false),
            State::BlockNodeOrIndentlessSequence => self.parse_node(event, true, true),
            State::FlowNode => self.parse_node(event, false, false),
            State::BlockSequenceFirstEntry => self.parse_block_sequence_entry(event, true),
            State::BlockSequenceEntry => self.parse_block_sequence_entry(event, false),
            State::IndentlessSequenceEntry => self.parse_indentless_sequence_entry(event),
            State::BlockMappingFirstKey => self.parse_block_mapping_key(event, true),
            State::BlockMappingKey => self.parse_block_mapping_key(event, false),
            State::BlockMappingValue => self.parse_block_mapping_value(event),
            State::FlowSequenceFirstEntry => self.parse_flow_sequence_entry(event, true),
            State::FlowSequenceEntry => self.parse_flow_sequence_entry(event, false),
            State::FlowSequenceEntryMappingKey => self.parse_flow_sequence_entry_mapping_key(event),
            State::FlowSequenceEntryMappingValue => {
                self.parse_flow_sequence_entry_mapping_value(event)
            }
            State::FlowSequenceEntryMappingEnd => self.parse_flow_sequence_entry_mapping_end(event),
            State::FlowMappingFirstKey => self.parse_flow_mapping_key(event, true),
            State::FlowMappingKey => self.parse_flow_mapping_key(event, false),
            State::FlowMappingValue => self.parse_flow_mapping_value(event, false),
            State::FlowMappingEmptyValue => self.parse_flow_mapping_value(event, true),
            // yaml.v3 panics with "invalid parser state" here; parse never
            // calls the state machine in the END state.
            State::End => self.set_parser_error("invalid parser state", self.mark),
        }
    }

    /// yaml_parser_parse_stream_start: parse the production
    ///
    /// ```text
    /// stream   ::= STREAM-START implicit_document? explicit_document* STREAM-END
    ///              ************
    /// ```
    fn parse_stream_start(&mut self, event: &mut Event) -> bool {
        let Some(token) = self.peek_token() else {
            return false;
        };
        if token.typ != TokenType::StreamStart {
            return self.set_parser_error("did not find expected <stream-start>", token.start_mark);
        }
        self.state = State::ImplicitDocumentStart;
        *event = Event {
            typ: EventType::StreamStart,
            start_mark: token.start_mark,
            end_mark: token.end_mark,
            encoding: token.encoding,
            ..Event::default()
        };
        self.skip_token();
        true
    }

    /// yaml_parser_parse_document_start: parse the productions
    ///
    /// ```text
    /// implicit_document    ::= block_node DOCUMENT-END*
    ///                          *
    /// explicit_document    ::= DIRECTIVE* DOCUMENT-START block_node? DOCUMENT-END*
    ///                          *************************
    /// ```
    fn parse_document_start(&mut self, event: &mut Event, implicit: bool) -> bool {
        let Some(mut token) = self.peek_token() else {
            return false;
        };

        // Parse extra document end indicators.
        if !implicit {
            while token.typ == TokenType::DocumentEnd {
                self.skip_token();
                let Some(next) = self.peek_token() else {
                    return false;
                };
                token = next;
            }
        }

        if implicit
            && token.typ != TokenType::VersionDirective
            && token.typ != TokenType::TagDirective
            && token.typ != TokenType::DocumentStart
            && token.typ != TokenType::StreamEnd
        {
            // Parse an implicit document.
            let (mut version_directive, mut tag_directives) = (None, Vec::new());
            if !self.process_directives(&mut version_directive, &mut tag_directives) {
                return false;
            }
            self.states.push(State::DocumentEnd);
            self.state = State::BlockNode;

            let mut head_comment = Vec::new();
            if let Some(last) = self.head_comment.len().checked_sub(1) {
                // [Go] Scan the header comment backwards, and if an empty
                //      line is found, break the header so the part before
                //      the last empty line goes into the document header,
                //      while the bottom of it goes into a follow up event.
                let mut i = last;
                while let Some(prev) = i.checked_sub(1) {
                    if at(&self.head_comment, i) == b'\n' {
                        let split = if i == last {
                            Some(i)
                        } else if at(&self.head_comment, prev) == b'\n' {
                            Some(prev)
                        } else {
                            None
                        };
                        if let Some(end) = split {
                            head_comment =
                                self.head_comment.get(..end).unwrap_or_default().to_vec();
                            self.head_comment =
                                self.head_comment.get(i + 1..).unwrap_or_default().to_vec();
                            break;
                        }
                    }
                    i = prev;
                }
            }

            *event = Event {
                typ: EventType::DocumentStart,
                start_mark: token.start_mark,
                end_mark: token.end_mark,
                head_comment,
                ..Event::default()
            };
        } else if token.typ != TokenType::StreamEnd {
            // Parse an explicit document.
            let mut version_directive = None;
            let mut tag_directives = Vec::new();
            let start_mark = token.start_mark;
            if !self.process_directives(&mut version_directive, &mut tag_directives) {
                return false;
            }
            let Some(token) = self.peek_token() else {
                return false;
            };
            if token.typ != TokenType::DocumentStart {
                self.set_parser_error("did not find expected <document start>", token.start_mark);
                return false;
            }
            self.states.push(State::DocumentEnd);
            self.state = State::DocumentContent;
            let end_mark = token.end_mark;

            *event = Event {
                typ: EventType::DocumentStart,
                start_mark,
                end_mark,
                version_directive,
                tag_directives,
                implicit: false,
                ..Event::default()
            };
            self.skip_token();
        } else {
            // Parse the stream end.
            self.state = State::End;
            *event = Event {
                typ: EventType::StreamEnd,
                start_mark: token.start_mark,
                end_mark: token.end_mark,
                ..Event::default()
            };
            self.skip_token();
        }

        true
    }

    /// yaml_parser_parse_document_content: parse the productions
    ///
    /// ```text
    /// explicit_document    ::= DIRECTIVE* DOCUMENT-START block_node? DOCUMENT-END*
    ///                                                    ***********
    /// ```
    fn parse_document_content(&mut self, event: &mut Event) -> bool {
        let Some(token) = self.peek_token() else {
            return false;
        };

        if token.typ == TokenType::VersionDirective
            || token.typ == TokenType::TagDirective
            || token.typ == TokenType::DocumentStart
            || token.typ == TokenType::DocumentEnd
            || token.typ == TokenType::StreamEnd
        {
            if !self.pop_state(token.start_mark) {
                return false;
            }
            return self.process_empty_scalar(event, token.start_mark);
        }
        self.parse_node(event, true, false)
    }

    /// yaml_parser_parse_document_end: parse the productions
    ///
    /// ```text
    /// implicit_document    ::= block_node DOCUMENT-END*
    ///                                     *************
    /// explicit_document    ::= DIRECTIVE* DOCUMENT-START block_node? DOCUMENT-END*
    /// ```
    fn parse_document_end(&mut self, event: &mut Event) -> bool {
        let Some(token) = self.peek_token() else {
            return false;
        };

        let start_mark = token.start_mark;
        let mut end_mark = token.start_mark;

        let mut implicit = true;
        if token.typ == TokenType::DocumentEnd {
            end_mark = token.end_mark;
            self.skip_token();
            implicit = false;
        }

        self.tag_directives.clear();

        self.state = State::DocumentStart;
        *event = Event {
            typ: EventType::DocumentEnd,
            start_mark,
            end_mark,
            implicit,
            ..Event::default()
        };
        self.set_event_comments(event);
        if !event.head_comment.is_empty() && event.foot_comment.is_empty() {
            event.foot_comment = mem::take(&mut event.head_comment);
        }
        true
    }

    /// yaml_parser_set_event_comments: move the parser's pending comments
    /// to the event.
    fn set_event_comments(&mut self, event: &mut Event) {
        event.head_comment = mem::take(&mut self.head_comment);
        event.line_comment = mem::take(&mut self.line_comment);
        event.foot_comment = mem::take(&mut self.foot_comment);
        self.tail_comment = Vec::new();
        self.stem_comment = Vec::new();
    }

    /// yaml_parser_parse_node: parse the productions
    ///
    /// ```text
    /// block_node_or_indentless_sequence    ::=
    ///                          ALIAS
    ///                          *****
    ///                          | properties (block_content | indentless_block_sequence)?
    ///                            **********  *
    ///                          | block_content | indentless_block_sequence
    ///                            *
    /// block_node           ::= ALIAS
    ///                          *****
    ///                          | properties block_content?
    ///                            ********** *
    ///                          | block_content
    ///                            *
    /// flow_node            ::= ALIAS
    ///                          *****
    ///                          | properties flow_content?
    ///                            ********** *
    ///                          | flow_content
    ///                            *
    /// properties           ::= TAG ANCHOR? | ANCHOR TAG?
    ///                          *************************
    /// block_content        ::= block_collection | flow_collection | SCALAR
    ///                                                               ******
    /// flow_content         ::= flow_collection | SCALAR
    ///                                            ******
    /// ```
    fn parse_node(&mut self, event: &mut Event, block: bool, indentless_sequence: bool) -> bool {
        let Some(mut token) = self.peek_token() else {
            return false;
        };

        if token.typ == TokenType::Alias {
            if !self.pop_state(token.start_mark) {
                return false;
            }
            *event = Event {
                typ: EventType::Alias,
                start_mark: token.start_mark,
                end_mark: token.end_mark,
                anchor: self.take_token_value(),
                ..Event::default()
            };
            self.set_event_comments(event);
            self.skip_token();
            return true;
        }

        let mut start_mark = token.start_mark;
        let mut end_mark = token.start_mark;

        let mut tag_token = false;
        let mut tag_handle = Vec::new();
        let mut tag_suffix = Vec::new();
        let mut anchor = Vec::new();
        let mut tag_mark = Mark::default();
        if token.typ == TokenType::Anchor {
            anchor = self.take_token_value();
            start_mark = token.start_mark;
            end_mark = token.end_mark;
            self.skip_token();
            let Some(next) = self.peek_token() else {
                return false;
            };
            token = next;
            if token.typ == TokenType::Tag {
                tag_token = true;
                tag_handle = self.take_token_value();
                tag_suffix = self.take_token_suffix();
                tag_mark = token.start_mark;
                end_mark = token.end_mark;
                self.skip_token();
                let Some(next) = self.peek_token() else {
                    return false;
                };
                token = next;
            }
        } else if token.typ == TokenType::Tag {
            tag_token = true;
            tag_handle = self.take_token_value();
            tag_suffix = self.take_token_suffix();
            start_mark = token.start_mark;
            tag_mark = token.start_mark;
            end_mark = token.end_mark;
            self.skip_token();
            let Some(next) = self.peek_token() else {
                return false;
            };
            token = next;
            if token.typ == TokenType::Anchor {
                anchor = self.take_token_value();
                end_mark = token.end_mark;
                self.skip_token();
                let Some(next) = self.peek_token() else {
                    return false;
                };
                token = next;
            }
        }

        let mut tag = Vec::new();
        if tag_token {
            if tag_handle.is_empty() {
                tag = tag_suffix;
            } else {
                if let Some(directive) = self.tag_directives.iter().find(|d| d.handle == tag_handle)
                {
                    tag = directive.prefix.clone();
                    tag.extend_from_slice(&tag_suffix);
                }
                if tag.is_empty() {
                    self.set_parser_error_context(
                        "while parsing a node",
                        start_mark,
                        "found undefined tag handle",
                        tag_mark,
                    );
                    return false;
                }
            }
        }

        let implicit = tag.is_empty();
        if indentless_sequence && token.typ == TokenType::BlockEntry {
            end_mark = token.end_mark;
            self.state = State::IndentlessSequenceEntry;
            *event = Event {
                typ: EventType::SequenceStart,
                start_mark,
                end_mark,
                anchor,
                tag,
                implicit,
                collection_style: CollectionStyle::Block,
                ..Event::default()
            };
            return true;
        }
        if token.typ == TokenType::Scalar {
            let mut plain_implicit = false;
            let mut quoted_implicit = false;
            end_mark = token.end_mark;
            if (tag.is_empty() && token.style == ScalarStyle::Plain) || tag == b"!" {
                plain_implicit = true;
            } else if tag.is_empty() {
                quoted_implicit = true;
            }
            if !self.pop_state(token.start_mark) {
                return false;
            }

            *event = Event {
                typ: EventType::Scalar,
                start_mark,
                end_mark,
                anchor,
                tag,
                value: self.take_token_value(),
                implicit: plain_implicit,
                quoted_implicit,
                scalar_style: token.style,
                ..Event::default()
            };
            self.set_event_comments(event);
            self.skip_token();
            return true;
        }
        if token.typ == TokenType::FlowSequenceStart {
            // [Go] Some of the events below can be merged as they differ
            //      only on style.
            end_mark = token.end_mark;
            self.state = State::FlowSequenceFirstEntry;
            *event = Event {
                typ: EventType::SequenceStart,
                start_mark,
                end_mark,
                anchor,
                tag,
                implicit,
                collection_style: CollectionStyle::Flow,
                ..Event::default()
            };
            self.set_event_comments(event);
            return true;
        }
        if token.typ == TokenType::FlowMappingStart {
            end_mark = token.end_mark;
            self.state = State::FlowMappingFirstKey;
            *event = Event {
                typ: EventType::MappingStart,
                start_mark,
                end_mark,
                anchor,
                tag,
                implicit,
                collection_style: CollectionStyle::Flow,
                ..Event::default()
            };
            self.set_event_comments(event);
            return true;
        }
        if block && token.typ == TokenType::BlockSequenceStart {
            end_mark = token.end_mark;
            self.state = State::BlockSequenceFirstEntry;
            *event = Event {
                typ: EventType::SequenceStart,
                start_mark,
                end_mark,
                anchor,
                tag,
                implicit,
                collection_style: CollectionStyle::Block,
                ..Event::default()
            };
            if !self.stem_comment.is_empty() {
                event.head_comment = mem::take(&mut self.stem_comment);
            }
            return true;
        }
        if block && token.typ == TokenType::BlockMappingStart {
            end_mark = token.end_mark;
            self.state = State::BlockMappingFirstKey;
            *event = Event {
                typ: EventType::MappingStart,
                start_mark,
                end_mark,
                anchor,
                tag,
                implicit,
                collection_style: CollectionStyle::Block,
                ..Event::default()
            };
            if !self.stem_comment.is_empty() {
                event.head_comment = mem::take(&mut self.stem_comment);
            }
            return true;
        }
        if !anchor.is_empty() || !tag.is_empty() {
            if !self.pop_state(token.start_mark) {
                return false;
            }

            *event = Event {
                typ: EventType::Scalar,
                start_mark,
                end_mark,
                anchor,
                tag,
                implicit,
                quoted_implicit: false,
                scalar_style: ScalarStyle::Plain,
                ..Event::default()
            };
            return true;
        }

        let context = if block {
            "while parsing a block node"
        } else {
            "while parsing a flow node"
        };
        self.set_parser_error_context(
            context,
            start_mark,
            "did not find expected node content",
            token.start_mark,
        );
        false
    }

    /// yaml_parser_parse_block_sequence_entry: parse the productions
    ///
    /// ```text
    /// block_sequence ::= BLOCK-SEQUENCE-START (BLOCK-ENTRY block_node?)* BLOCK-END
    ///                    ********************  *********** *             *********
    /// ```
    fn parse_block_sequence_entry(&mut self, event: &mut Event, first: bool) -> bool {
        if first {
            let Some(token) = self.peek_token() else {
                return false;
            };
            self.marks.push(token.start_mark);
            self.skip_token();
        }

        let Some(mut token) = self.peek_token() else {
            return false;
        };

        if token.typ == TokenType::BlockEntry {
            let mark = token.end_mark;
            let prior_head_len = self.head_comment.len();
            self.skip_token();
            self.split_stem_comment(prior_head_len);
            let Some(next) = self.peek_token() else {
                return false;
            };
            token = next;
            if token.typ != TokenType::BlockEntry && token.typ != TokenType::BlockEnd {
                self.states.push(State::BlockSequenceEntry);
                return self.parse_node(event, true, false);
            }
            self.state = State::BlockSequenceEntry;
            return self.process_empty_scalar(event, mark);
        }
        if token.typ == TokenType::BlockEnd {
            if !self.pop_state(token.start_mark) || !self.pop_mark(token.start_mark) {
                return false;
            }

            *event = Event {
                typ: EventType::SequenceEnd,
                start_mark: token.start_mark,
                end_mark: token.end_mark,
                ..Event::default()
            };

            self.skip_token();
            return true;
        }

        let context_mark = self.marks.pop().unwrap_or_default();
        self.set_parser_error_context(
            "while parsing a block collection",
            context_mark,
            "did not find expected '-' indicator",
            token.start_mark,
        )
    }

    /// yaml_parser_parse_indentless_sequence_entry: parse the productions
    ///
    /// ```text
    /// indentless_sequence  ::= (BLOCK-ENTRY block_node?)+
    ///                           *********** *
    /// ```
    fn parse_indentless_sequence_entry(&mut self, event: &mut Event) -> bool {
        let Some(mut token) = self.peek_token() else {
            return false;
        };

        if token.typ == TokenType::BlockEntry {
            let mark = token.end_mark;
            let prior_head_len = self.head_comment.len();
            self.skip_token();
            self.split_stem_comment(prior_head_len);
            let Some(next) = self.peek_token() else {
                return false;
            };
            token = next;
            if token.typ != TokenType::BlockEntry
                && token.typ != TokenType::Key
                && token.typ != TokenType::Value
                && token.typ != TokenType::BlockEnd
            {
                self.states.push(State::IndentlessSequenceEntry);
                return self.parse_node(event, true, false);
            }
            self.state = State::IndentlessSequenceEntry;
            return self.process_empty_scalar(event, mark);
        }
        if !self.pop_state(token.start_mark) {
            return false;
        }

        *event = Event {
            typ: EventType::SequenceEnd,
            start_mark: token.start_mark,
            end_mark: token.start_mark, // [Go] Shouldn't this be token.end_mark?
            ..Event::default()
        };
        true
    }

    /// yaml_parser_split_stem_comment: split stem comment from head
    /// comment.
    ///
    /// When a sequence or map is found under a sequence entry, the former
    /// head comment is assigned to the underlying sequence or map as a
    /// whole, not the individual sequence or map entry as would be expected
    /// otherwise. To handle this case the previous head comment is moved
    /// aside as the stem comment.
    fn split_stem_comment(&mut self, stem_len: usize) {
        if stem_len == 0 {
            return;
        }

        let Some(token) = self.peek_token() else {
            return;
        };
        if token.typ != TokenType::BlockSequenceStart && token.typ != TokenType::BlockMappingStart {
            return;
        }

        self.stem_comment = self
            .head_comment
            .get(..stem_len)
            .unwrap_or_default()
            .to_vec();
        if self.head_comment.len() == stem_len {
            self.head_comment = Vec::new();
        } else {
            self.head_comment = self
                .head_comment
                .get(stem_len + 1..)
                .unwrap_or_default()
                .to_vec();
        }
    }

    /// yaml_parser_parse_block_mapping_key: parse the productions
    ///
    /// ```text
    /// block_mapping        ::= BLOCK-MAPPING_START
    ///                          *******************
    ///                          ((KEY block_node_or_indentless_sequence?)?
    ///                            *** *
    ///                          (VALUE block_node_or_indentless_sequence?)?)*
    ///
    ///                          BLOCK-END
    ///                          *********
    /// ```
    fn parse_block_mapping_key(&mut self, event: &mut Event, first: bool) -> bool {
        if first {
            let Some(token) = self.peek_token() else {
                return false;
            };
            self.marks.push(token.start_mark);
            self.skip_token();
        }

        let Some(mut token) = self.peek_token() else {
            return false;
        };

        // [Go] A tail comment was left from the prior mapping value
        //      processed. Emit an event as it needs to be processed with that
        //      value and not the following key.
        if !self.tail_comment.is_empty() {
            *event = Event {
                typ: EventType::TailComment,
                start_mark: token.start_mark,
                end_mark: token.end_mark,
                foot_comment: mem::take(&mut self.tail_comment),
                ..Event::default()
            };
            return true;
        }

        if token.typ == TokenType::Key {
            let mark = token.end_mark;
            self.skip_token();
            let Some(next) = self.peek_token() else {
                return false;
            };
            token = next;
            if token.typ != TokenType::Key
                && token.typ != TokenType::Value
                && token.typ != TokenType::BlockEnd
            {
                self.states.push(State::BlockMappingValue);
                return self.parse_node(event, true, true);
            }
            self.state = State::BlockMappingValue;
            return self.process_empty_scalar(event, mark);
        } else if token.typ == TokenType::BlockEnd {
            if !self.pop_state(token.start_mark) || !self.pop_mark(token.start_mark) {
                return false;
            }
            *event = Event {
                typ: EventType::MappingEnd,
                start_mark: token.start_mark,
                end_mark: token.end_mark,
                ..Event::default()
            };
            self.set_event_comments(event);
            self.skip_token();
            return true;
        }

        let context_mark = self.marks.pop().unwrap_or_default();
        self.set_parser_error_context(
            "while parsing a block mapping",
            context_mark,
            "did not find expected key",
            token.start_mark,
        )
    }

    /// yaml_parser_parse_block_mapping_value: parse the productions
    ///
    /// ```text
    /// block_mapping        ::= BLOCK-MAPPING_START
    ///
    ///                          ((KEY block_node_or_indentless_sequence?)?
    ///
    ///                          (VALUE block_node_or_indentless_sequence?)?)*
    ///                           ***** *
    ///                          BLOCK-END
    /// ```
    fn parse_block_mapping_value(&mut self, event: &mut Event) -> bool {
        let Some(token) = self.peek_token() else {
            return false;
        };
        if token.typ == TokenType::Value {
            let mark = token.end_mark;
            self.skip_token();
            let Some(token) = self.peek_token() else {
                return false;
            };
            if token.typ != TokenType::Key
                && token.typ != TokenType::Value
                && token.typ != TokenType::BlockEnd
            {
                self.states.push(State::BlockMappingKey);
                return self.parse_node(event, true, true);
            }
            self.state = State::BlockMappingKey;
            return self.process_empty_scalar(event, mark);
        }
        self.state = State::BlockMappingKey;
        self.process_empty_scalar(event, token.start_mark)
    }

    /// yaml_parser_parse_flow_sequence_entry: parse the productions
    ///
    /// ```text
    /// flow_sequence        ::= FLOW-SEQUENCE-START
    ///                          *******************
    ///                          (flow_sequence_entry FLOW-ENTRY)*
    ///                           *                   **********
    ///                          flow_sequence_entry?
    ///                          *
    ///                          FLOW-SEQUENCE-END
    ///                          *****************
    /// flow_sequence_entry  ::= flow_node | KEY flow_node? (VALUE flow_node?)?
    ///                          *
    /// ```
    fn parse_flow_sequence_entry(&mut self, event: &mut Event, first: bool) -> bool {
        if first {
            let Some(token) = self.peek_token() else {
                return false;
            };
            self.marks.push(token.start_mark);
            self.skip_token();
        }
        let Some(mut token) = self.peek_token() else {
            return false;
        };
        if token.typ != TokenType::FlowSequenceEnd {
            if !first {
                if token.typ == TokenType::FlowEntry {
                    self.skip_token();
                    let Some(next) = self.peek_token() else {
                        return false;
                    };
                    token = next;
                } else {
                    let context_mark = self.marks.pop().unwrap_or_default();
                    return self.set_parser_error_context(
                        "while parsing a flow sequence",
                        context_mark,
                        "did not find expected ',' or ']'",
                        token.start_mark,
                    );
                }
            }

            if token.typ == TokenType::Key {
                self.state = State::FlowSequenceEntryMappingKey;
                *event = Event {
                    typ: EventType::MappingStart,
                    start_mark: token.start_mark,
                    end_mark: token.end_mark,
                    implicit: true,
                    collection_style: CollectionStyle::Flow,
                    ..Event::default()
                };
                self.skip_token();
                return true;
            } else if token.typ != TokenType::FlowSequenceEnd {
                self.states.push(State::FlowSequenceEntry);
                return self.parse_node(event, false, false);
            }
        }

        if !self.pop_state(token.start_mark) || !self.pop_mark(token.start_mark) {
            return false;
        }

        *event = Event {
            typ: EventType::SequenceEnd,
            start_mark: token.start_mark,
            end_mark: token.end_mark,
            ..Event::default()
        };
        self.set_event_comments(event);

        self.skip_token();
        true
    }

    /// yaml_parser_parse_flow_sequence_entry_mapping_key: parse the
    /// productions
    ///
    /// ```text
    /// flow_sequence_entry  ::= flow_node | KEY flow_node? (VALUE flow_node?)?
    ///                                      *** *
    /// ```
    fn parse_flow_sequence_entry_mapping_key(&mut self, event: &mut Event) -> bool {
        let Some(token) = self.peek_token() else {
            return false;
        };
        if token.typ != TokenType::Value
            && token.typ != TokenType::FlowEntry
            && token.typ != TokenType::FlowSequenceEnd
        {
            self.states.push(State::FlowSequenceEntryMappingValue);
            return self.parse_node(event, false, false);
        }
        let mark = token.end_mark;
        self.skip_token();
        self.state = State::FlowSequenceEntryMappingValue;
        self.process_empty_scalar(event, mark)
    }

    /// yaml_parser_parse_flow_sequence_entry_mapping_value: parse the
    /// productions
    ///
    /// ```text
    /// flow_sequence_entry  ::= flow_node | KEY flow_node? (VALUE flow_node?)?
    ///                                                      ***** *
    /// ```
    fn parse_flow_sequence_entry_mapping_value(&mut self, event: &mut Event) -> bool {
        let Some(token) = self.peek_token() else {
            return false;
        };
        if token.typ == TokenType::Value {
            self.skip_token();
            // yaml.v3 shadows token here, so the empty scalar below still
            // gets the VALUE token's mark.
            let Some(token) = self.peek_token() else {
                return false;
            };
            if token.typ != TokenType::FlowEntry && token.typ != TokenType::FlowSequenceEnd {
                self.states.push(State::FlowSequenceEntryMappingEnd);
                return self.parse_node(event, false, false);
            }
        }
        self.state = State::FlowSequenceEntryMappingEnd;
        self.process_empty_scalar(event, token.start_mark)
    }

    /// yaml_parser_parse_flow_sequence_entry_mapping_end: parse the
    /// productions
    ///
    /// ```text
    /// flow_sequence_entry  ::= flow_node | KEY flow_node? (VALUE flow_node?)?
    ///                                                                      *
    /// ```
    fn parse_flow_sequence_entry_mapping_end(&mut self, event: &mut Event) -> bool {
        let Some(token) = self.peek_token() else {
            return false;
        };
        self.state = State::FlowSequenceEntry;
        *event = Event {
            typ: EventType::MappingEnd,
            start_mark: token.start_mark,
            end_mark: token.start_mark, // [Go] Shouldn't this be end_mark?
            ..Event::default()
        };
        true
    }

    /// yaml_parser_parse_flow_mapping_key: parse the productions
    ///
    /// ```text
    /// flow_mapping         ::= FLOW-MAPPING-START
    ///                          ******************
    ///                          (flow_mapping_entry FLOW-ENTRY)*
    ///                           *                  **********
    ///                          flow_mapping_entry?
    ///                          ******************
    ///                          FLOW-MAPPING-END
    ///                          ****************
    /// flow_mapping_entry   ::= flow_node | KEY flow_node? (VALUE flow_node?)?
    ///                          *           *** *
    /// ```
    fn parse_flow_mapping_key(&mut self, event: &mut Event, first: bool) -> bool {
        if first {
            // yaml.v3 doesn't check for nil here; see the module docs.
            let Some(token) = self.peek_token() else {
                return false;
            };
            self.marks.push(token.start_mark);
            self.skip_token();
        }

        let Some(mut token) = self.peek_token() else {
            return false;
        };

        if token.typ != TokenType::FlowMappingEnd {
            if !first {
                if token.typ == TokenType::FlowEntry {
                    self.skip_token();
                    let Some(next) = self.peek_token() else {
                        return false;
                    };
                    token = next;
                } else {
                    let context_mark = self.marks.pop().unwrap_or_default();
                    return self.set_parser_error_context(
                        "while parsing a flow mapping",
                        context_mark,
                        "did not find expected ',' or '}'",
                        token.start_mark,
                    );
                }
            }

            if token.typ == TokenType::Key {
                self.skip_token();
                let Some(next) = self.peek_token() else {
                    return false;
                };
                token = next;
                if token.typ != TokenType::Value
                    && token.typ != TokenType::FlowEntry
                    && token.typ != TokenType::FlowMappingEnd
                {
                    self.states.push(State::FlowMappingValue);
                    return self.parse_node(event, false, false);
                }
                self.state = State::FlowMappingValue;
                return self.process_empty_scalar(event, token.start_mark);
            } else if token.typ != TokenType::FlowMappingEnd {
                self.states.push(State::FlowMappingEmptyValue);
                return self.parse_node(event, false, false);
            }
        }

        if !self.pop_state(token.start_mark) || !self.pop_mark(token.start_mark) {
            return false;
        }
        *event = Event {
            typ: EventType::MappingEnd,
            start_mark: token.start_mark,
            end_mark: token.end_mark,
            ..Event::default()
        };
        self.set_event_comments(event);
        self.skip_token();
        true
    }

    /// yaml_parser_parse_flow_mapping_value: parse the productions
    ///
    /// ```text
    /// flow_mapping_entry   ::= flow_node | KEY flow_node? (VALUE flow_node?)?
    ///                                   *                  ***** *
    /// ```
    fn parse_flow_mapping_value(&mut self, event: &mut Event, empty: bool) -> bool {
        let Some(mut token) = self.peek_token() else {
            return false;
        };
        if empty {
            self.state = State::FlowMappingKey;
            return self.process_empty_scalar(event, token.start_mark);
        }
        if token.typ == TokenType::Value {
            self.skip_token();
            let Some(next) = self.peek_token() else {
                return false;
            };
            token = next;
            if token.typ != TokenType::FlowEntry && token.typ != TokenType::FlowMappingEnd {
                self.states.push(State::FlowMappingKey);
                return self.parse_node(event, false, false);
            }
        }
        self.state = State::FlowMappingKey;
        self.process_empty_scalar(event, token.start_mark)
    }

    /// yaml_parser_process_empty_scalar: generate an empty scalar event.
    fn process_empty_scalar(&mut self, event: &mut Event, mark: Mark) -> bool {
        *event = Event {
            typ: EventType::Scalar,
            start_mark: mark,
            end_mark: mark,
            value: Vec::new(), // Empty
            implicit: true,
            scalar_style: ScalarStyle::Plain,
            ..Event::default()
        };
        true
    }

    /// yaml_parser_process_directives: parse directives.
    fn process_directives(
        &mut self,
        version_directive_ref: &mut Option<VersionDirective>,
        tag_directives_ref: &mut Vec<TagDirective>,
    ) -> bool {
        let mut version_directive: Option<VersionDirective> = None;
        let mut tag_directives: Vec<TagDirective> = Vec::new();

        let Some(mut token) = self.peek_token() else {
            return false;
        };

        while token.typ == TokenType::VersionDirective || token.typ == TokenType::TagDirective {
            if token.typ == TokenType::VersionDirective {
                if version_directive.is_some() {
                    self.set_parser_error("found duplicate %YAML directive", token.start_mark);
                    return false;
                }
                if token.major != 1 || token.minor != 1 {
                    self.set_parser_error("found incompatible YAML document", token.start_mark);
                    return false;
                }
                version_directive = Some(VersionDirective {
                    major: token.major,
                    minor: token.minor,
                });
            } else if token.typ == TokenType::TagDirective {
                let value = TagDirective {
                    handle: self.take_token_value(),
                    prefix: self.take_token_prefix(),
                };
                if !self.append_tag_directive(&value, false, token.start_mark) {
                    return false;
                }
                tag_directives.push(value);
            }

            self.skip_token();
            let Some(next) = self.peek_token() else {
                return false;
            };
            token = next;
        }

        for (handle, prefix) in DEFAULT_TAG_DIRECTIVES {
            let value = TagDirective {
                handle: handle.to_vec(),
                prefix: prefix.to_vec(),
            };
            if !self.append_tag_directive(&value, true, token.start_mark) {
                return false;
            }
        }

        *version_directive_ref = version_directive;
        *tag_directives_ref = tag_directives;
        true
    }

    /// yaml_parser_append_tag_directive: append a tag directive to the
    /// directives stack.
    fn append_tag_directive(
        &mut self,
        value: &TagDirective,
        allow_duplicates: bool,
        mark: Mark,
    ) -> bool {
        if self.tag_directives.iter().any(|d| d.handle == value.handle) {
            if allow_duplicates {
                return true;
            }
            return self.set_parser_error("found duplicate %TAG directive", mark);
        }

        self.tag_directives.push(value.clone());
        true
    }
}

#[cfg(test)]
mod tests {
    //! The parser and scanner against yaml.v3's own answers, recorded with
    //! the Go probe (`events-raw` and `tokens` modes) in
    //! `src/config/testdata/yaml3/parser_events.json`.
    //!
    //! Each recorded case has a name `n`, an input `in` (`s`: text, `b`:
    //! base64 bytes, `r`/`rb`: runs of `[piece, count]`, text or base64),
    //! the events `ev` as compact rows (see [`compact_event`]) or, for long
    //! outputs, a digest `evd`, the error `err` (see [`compact_error`]) and
    //! a digest `tk` of the tokens and the token error. Cases named `lit N`
    //! are the string literals of yaml.v3's decode, encode, node and limit
    //! tests; `mut N` are seeded random mutations; the rest are written by
    //! hand.

    use super::*;
    use crate::config::yaml3::types::Token;
    use serde::Deserialize;
    use serde_json::{Map, Value, json};

    /// The recorded cases.
    const DATA: &str = include_str!("../testdata/yaml3/parser_events.json");

    /// A recorded case.
    #[derive(Deserialize)]
    struct Case {
        /// The case name.
        n: String,
        /// The input.
        #[serde(rename = "in")]
        input: Input,
        /// yaml.v3's events as compact rows.
        ev: Option<Vec<Value>>,
        /// A digest of yaml.v3's events, for long outputs.
        evd: Option<Digest>,
        /// yaml.v3's parse error as a compact row.
        err: Option<Value>,
        /// `count:fnv` of yaml.v3's tokens and scan error.
        tk: Option<String>,
        /// A Go panic while parsing (none are recorded).
        go_panic: Option<String>,
        /// A Go panic while scanning (none are recorded).
        tk_go_panic: Option<String>,
    }

    /// A recorded input.
    #[derive(Deserialize)]
    struct Input {
        /// UTF-8 text.
        s: Option<String>,
        /// Base64 bytes.
        b: Option<String>,
        /// Runs of text.
        r: Option<Vec<(String, usize)>>,
        /// Runs of base64 bytes.
        rb: Option<Vec<(String, usize)>>,
    }

    impl Input {
        /// The input bytes.
        fn bytes(&self) -> Vec<u8> {
            if let Some(s) = &self.s {
                return s.as_bytes().to_vec();
            }
            if let Some(b) = &self.b {
                return base64(b);
            }
            if let Some(runs) = &self.r {
                return runs
                    .iter()
                    .flat_map(|(p, n)| p.as_bytes().repeat(*n))
                    .collect();
            }
            if let Some(runs) = &self.rb {
                return runs
                    .iter()
                    .flat_map(|(p, n)| base64(p).repeat(*n))
                    .collect();
            }
            panic!("input without data");
        }
    }

    /// A digest of a long event list.
    #[derive(Deserialize)]
    struct Digest {
        /// The number of events.
        n: usize,
        /// The first events (null where too long to keep).
        first: Vec<Value>,
        /// The last events (null where too long to keep).
        last: Vec<Value>,
        /// FNV-1a 64 of the canonical lines (see [`canon_event`]).
        fnv: String,
    }

    /// The recorded cases.
    fn cases() -> Vec<Case> {
        serde_json::from_str(DATA).expect("parser_events.json")
    }

    /// Decode standard base64.
    fn base64(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let (mut acc, mut bits) = (0u32, 0u32);
        for c in s.bytes() {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => continue,
                _ => panic!("bad base64"),
            };
            acc = (acc << 6) | u32::from(v);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(u8::try_from(acc >> bits).expect("byte"));
                acc &= (1 << bits) - 1;
            }
        }
        out
    }

    /// Bytes as Go's encoding/json writes a `string(bytes)`: each byte of
    /// an invalid UTF-8 sequence becomes U+FFFD.
    fn go_string(b: &[u8]) -> String {
        let mut out = String::new();
        let mut rest = b;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    return out;
                }
                Err(e) => {
                    let (valid, after) = rest.split_at(e.valid_up_to());
                    out.push_str(std::str::from_utf8(valid).expect("valid prefix"));
                    out.push(char::REPLACEMENT_CHARACTER);
                    rest = &after[1..];
                }
            }
        }
    }

    /// yaml_event_type_t's number.
    fn event_code(t: EventType) -> u8 {
        match t {
            EventType::No => 0,
            EventType::StreamStart => 1,
            EventType::StreamEnd => 2,
            EventType::DocumentStart => 3,
            EventType::DocumentEnd => 4,
            EventType::Alias => 5,
            EventType::Scalar => 6,
            EventType::SequenceStart => 7,
            EventType::SequenceEnd => 8,
            EventType::MappingStart => 9,
            EventType::MappingEnd => 10,
            EventType::TailComment => 11,
        }
    }

    /// yaml_token_type_t's number.
    fn token_code(t: TokenType) -> u8 {
        match t {
            TokenType::No => 0,
            TokenType::StreamStart => 1,
            TokenType::StreamEnd => 2,
            TokenType::VersionDirective => 3,
            TokenType::TagDirective => 4,
            TokenType::DocumentStart => 5,
            TokenType::DocumentEnd => 6,
            TokenType::BlockSequenceStart => 7,
            TokenType::BlockMappingStart => 8,
            TokenType::BlockEnd => 9,
            TokenType::FlowSequenceStart => 10,
            TokenType::FlowSequenceEnd => 11,
            TokenType::FlowMappingStart => 12,
            TokenType::FlowMappingEnd => 13,
            TokenType::BlockEntry => 14,
            TokenType::FlowEntry => 15,
            TokenType::Key => 16,
            TokenType::Value => 17,
            TokenType::Alias => 18,
            TokenType::Anchor => 19,
            TokenType::Tag => 20,
            TokenType::Scalar => 21,
        }
    }

    /// yaml_encoding_t's number.
    fn encoding_code(e: Encoding) -> u8 {
        match e {
            Encoding::Any => 0,
            Encoding::Utf8 => 1,
            Encoding::Utf16Le => 2,
            Encoding::Utf16Be => 3,
        }
    }

    /// yaml_scalar_style_t's number.
    fn scalar_code(s: ScalarStyle) -> u8 {
        match s {
            ScalarStyle::Any => 0,
            ScalarStyle::Plain => 2,
            ScalarStyle::SingleQuoted => 4,
            ScalarStyle::DoubleQuoted => 8,
            ScalarStyle::Literal => 16,
            ScalarStyle::Folded => 32,
        }
    }

    /// yaml_sequence_style_t / yaml_mapping_style_t's number.
    fn collection_code(s: CollectionStyle) -> u8 {
        match s {
            CollectionStyle::Any => 0,
            CollectionStyle::Block => 1,
            CollectionStyle::Flow => 2,
        }
    }

    /// yaml_error_type_t's number.
    fn error_code(e: ErrorType) -> u8 {
        match e {
            ErrorType::No => 0,
            ErrorType::Memory => 1,
            ErrorType::Reader => 2,
            ErrorType::Scanner => 3,
            ErrorType::Parser => 4,
            ErrorType::Composer => 5,
            ErrorType::Writer => 6,
            ErrorType::Emitter => 7,
        }
    }

    /// A mark as the probe writes it.
    fn mark(m: Mark) -> Value {
        json!([m.index, m.line, m.column])
    }

    /// The style number the probe writes for an event (yaml.v3 has one
    /// style field), and whether the other style is set when it shouldn't
    /// be.
    fn event_style(ev: &Event) -> (u8, bool) {
        match ev.typ {
            EventType::Scalar => (
                scalar_code(ev.scalar_style),
                ev.collection_style != CollectionStyle::Any,
            ),
            EventType::SequenceStart | EventType::MappingStart => (
                collection_code(ev.collection_style),
                ev.scalar_style != ScalarStyle::Any,
            ),
            _ => (
                0,
                ev.scalar_style != ScalarStyle::Any || ev.collection_style != CollectionStyle::Any,
            ),
        }
    }

    /// An event as a compact row: `[type, start index, line, column, end
    /// index, line, column, {extras}]`, the extras (omitted when empty)
    /// being e encoding, V version, g tag directives, h/l/f/T head, line,
    /// foot and tail comments, a anchor, t tag, v value, i implicit, q
    /// quoted_implicit and s style, each only when not empty or zero.
    fn compact_event(ev: &Event) -> Value {
        let s = ev.start_mark;
        let e = ev.end_mark;
        let mut row = vec![
            json!(event_code(ev.typ)),
            json!(s.index),
            json!(s.line),
            json!(s.column),
            json!(e.index),
            json!(e.line),
            json!(e.column),
        ];
        let mut extra = Map::new();
        let enc = encoding_code(ev.encoding);
        if enc != 0 {
            extra.insert("e".into(), json!(enc));
        }
        if let Some(v) = ev.version_directive {
            extra.insert("V".into(), json!([v.major, v.minor]));
        }
        if !ev.tag_directives.is_empty() {
            let tags: Vec<Value> = ev
                .tag_directives
                .iter()
                .map(|t| json!([go_string(&t.handle), go_string(&t.prefix)]))
                .collect();
            extra.insert("g".into(), Value::Array(tags));
        }
        for (key, text) in [
            ("h", &ev.head_comment),
            ("l", &ev.line_comment),
            ("f", &ev.foot_comment),
            ("T", &ev.tail_comment),
            ("a", &ev.anchor),
            ("t", &ev.tag),
            ("v", &ev.value),
        ] {
            if !text.is_empty() {
                extra.insert(key.into(), json!(go_string(text)));
            }
        }
        if ev.implicit {
            extra.insert("i".into(), json!(1));
        }
        if ev.quoted_implicit {
            extra.insert("q".into(), json!(1));
        }
        let (style, stray) = event_style(ev);
        if style != 0 {
            extra.insert("s".into(), json!(style));
        }
        if stray {
            extra.insert("stray style".into(), json!(true));
        }
        if !extra.is_empty() {
            row.push(Value::Object(extra));
        }
        Value::Array(row)
    }

    /// An event as the recorder's canonical digest line.
    fn canon_event(ev: &Event) -> String {
        let s = ev.start_mark;
        let e = ev.end_mark;
        let version = ev
            .version_directive
            .map(|v| format!("{}.{}", v.major, v.minor))
            .unwrap_or_default();
        let tags: Vec<String> = ev
            .tag_directives
            .iter()
            .map(|t| format!("{} {}", go_string(&t.handle), go_string(&t.prefix)))
            .collect();
        let (style, stray) = event_style(ev);
        format!(
            "{}|{},{},{}|{},{},{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}{}\n",
            event_code(ev.typ),
            s.index,
            s.line,
            s.column,
            e.index,
            e.line,
            e.column,
            encoding_code(ev.encoding),
            version,
            tags.join(";"),
            go_string(&ev.head_comment),
            go_string(&ev.line_comment),
            go_string(&ev.foot_comment),
            go_string(&ev.tail_comment),
            go_string(&ev.anchor),
            go_string(&ev.tag),
            go_string(&ev.value),
            u8::from(ev.implicit),
            u8::from(ev.quoted_implicit),
            style,
            if stray { "|stray style" } else { "" },
        )
    }

    /// A token as the recorder's canonical digest line.
    fn canon_token(t: &Token) -> String {
        let s = t.start_mark;
        let e = t.end_mark;
        format!(
            "{}|{},{},{}|{},{},{}|{}|{}|{}|{}|{}|{}\n",
            token_code(t.typ),
            s.index,
            s.line,
            s.column,
            e.index,
            e.line,
            e.column,
            go_string(&t.value),
            go_string(&t.suffix),
            go_string(&t.prefix),
            scalar_code(t.style),
            t.major,
            t.minor,
        )
    }

    /// An error as the recorder's canonical digest line.
    fn canon_error(e: &ParserError) -> String {
        let p = e.problem_mark;
        let c = e.context_mark;
        format!(
            "E|{}|{}|{}|{}|{},{},{}|{}|{},{},{}\n",
            error_code(e.kind),
            e.problem,
            e.problem_offset,
            e.problem_value,
            p.index,
            p.line,
            p.column,
            e.context,
            c.index,
            c.line,
            c.column,
        )
    }

    /// FNV-1a 64, as hex.
    fn fnv(text: &str) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in text.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{h:016x}")
    }

    /// An error as a compact row: `[kind, problem, problem_offset,
    /// problem_value, problem_mark, context, context_mark, message]`, the
    /// message being decode.go's (`"yaml: "` and [`ParserError::message`]).
    fn compact_error(e: &ParserError) -> Value {
        json!([
            error_code(e.kind),
            e.problem,
            e.problem_offset,
            e.problem_value,
            mark(e.problem_mark),
            e.context,
            mark(e.context_mark),
            format!("yaml: {}", e.message()),
        ])
    }

    /// Parse as the probe does: events up to STREAM-END (or a NO event) or
    /// the first error.
    fn run_events(input: &[u8]) -> (Vec<Event>, Option<ParserError>) {
        let mut parser = Parser::new(input);
        let mut events = Vec::new();
        loop {
            match parser.parse() {
                Ok(ev) => {
                    let done = matches!(ev.typ, EventType::StreamEnd | EventType::No);
                    events.push(ev);
                    if done {
                        return (events, None);
                    }
                }
                Err(e) => return (events, Some(e)),
            }
        }
    }

    /// Scan as the probe does: tokens up to STREAM-END (or a NO token) or
    /// the first error.
    fn run_tokens(input: &[u8]) -> (Vec<Token>, Option<ParserError>) {
        let mut parser = Parser::new(input);
        let mut tokens = Vec::new();
        loop {
            match parser.scan() {
                Ok(t) => {
                    let done = matches!(t.typ, TokenType::StreamEnd | TokenType::No);
                    tokens.push(t);
                    if done {
                        return (tokens, None);
                    }
                }
                Err(e) => return (tokens, Some(e)),
            }
        }
    }

    /// Compare sample rows of a digest (nulls skipped).
    fn check_samples(name: &str, want: &[Value], got: &[Event]) -> Result<(), String> {
        for (i, (w, g)) in want.iter().zip(got).enumerate() {
            let row = compact_event(g);
            if !w.is_null() && *w != row {
                return Err(format!("{name}: sample {i}: want {w}, got {row}"));
            }
        }
        Ok(())
    }

    /// Check one case against yaml.v3's recorded events and error.
    fn check_events(case: &Case) -> Result<(), String> {
        let name = &case.n;
        if case.go_panic.is_some() {
            return Err(format!("{name}: Go panicked; record a Rust expectation"));
        }
        let (events, err) = run_events(&case.input.bytes());
        if let Some(want) = &case.ev {
            if want.len() != events.len() {
                let (w, g) = (want.len(), events.len());
                return Err(format!("{name}: want {w} events, got {g}"));
            }
            for (i, (w, ev)) in want.iter().zip(&events).enumerate() {
                let got = compact_event(ev);
                if *w != got {
                    return Err(format!("{name}: event {i}: want {w}, got {got}"));
                }
            }
        } else if let Some(d) = &case.evd {
            if d.n != events.len() {
                let (w, g) = (d.n, events.len());
                return Err(format!("{name}: want {w} events, got {g}"));
            }
            check_samples(name, &d.first, &events)?;
            let tail = events.len().saturating_sub(d.last.len());
            check_samples(name, &d.last, &events[tail..])?;
            let text: String = events.iter().map(canon_event).collect();
            if fnv(&text) != d.fnv {
                return Err(format!("{name}: event digest differs"));
            }
        } else {
            return Err(format!("{name}: no recorded events"));
        }
        let got_err = err.as_ref().map(compact_error);
        if case.err != got_err {
            let want = &case.err;
            return Err(format!("{name}: error: want {want:?}, got {got_err:?}"));
        }
        Ok(())
    }

    /// Check one case against yaml.v3's recorded tokens and error.
    fn check_tokens(case: &Case) -> Result<(), String> {
        let name = &case.n;
        if case.tk_go_panic.is_some() {
            return Err(format!("{name}: Go panicked; record a Rust expectation"));
        }
        let Some(want) = &case.tk else {
            return Err(format!("{name}: no recorded tokens"));
        };
        let (tokens, err) = run_tokens(&case.input.bytes());
        let mut text: String = tokens.iter().map(canon_token).collect();
        if let Some(e) = &err {
            text.push_str(&canon_error(e));
        }
        let got = format!("{}:{}", tokens.len(), fnv(&text));
        if *want != got {
            return Err(format!("{name}: tokens: want {want}, got {got}"));
        }
        Ok(())
    }

    /// Run `check` over the cases whose names pass `filter`, failing with
    /// the first few mismatches; the number of cases checked.
    fn check_all(
        cases: &[Case],
        filter: impl Fn(&str) -> bool,
        check: fn(&Case) -> Result<(), String>,
    ) -> usize {
        let mut failures = Vec::new();
        let mut count = 0;
        for case in cases.iter().filter(|c| filter(&c.n)) {
            count += 1;
            if let Err(e) = check(case) {
                failures.push(e);
            }
        }
        let shown: Vec<String> = failures.iter().take(10).cloned().collect();
        assert!(
            failures.is_empty(),
            "{} of {count} cases differ from yaml.v3:\n{}",
            failures.len(),
            shown.join("\n")
        );
        count
    }

    /// Is this a yaml.v3 test literal?
    fn is_literal(name: &str) -> bool {
        name.starts_with("lit ")
    }

    /// Is this a stored random mutation?
    fn is_mutation(name: &str) -> bool {
        name.starts_with("mut ")
    }

    // Ported from yaml.v3 decode_test.go, encode_test.go, node_test.go and
    // limit_test.go: every string literal in them, parsed raw, against the
    // events-raw recordings.
    #[test]
    fn events_of_yaml_v3_test_literals() {
        let n = check_all(&cases(), is_literal, check_events);
        assert!(n > 700);
    }

    // Not upstream's: hand-written cases for every scanner and parser
    // feature and every reachable error message, against the events-raw
    // recordings.
    #[test]
    fn events_of_hand_written_cases() {
        let hand = |n: &str| !is_literal(n) && !is_mutation(n);
        let n = check_all(&cases(), hand, check_events);
        assert!(n > 350);
    }

    // Not upstream's: seeded random mutations of the other cases, against
    // the events-raw recordings.
    #[test]
    fn events_of_mutations() {
        let n = check_all(&cases(), is_mutation, check_events);
        assert!(n >= 200);
    }

    // Not upstream's: every recorded case, scanned token by token, against
    // the tokens recordings.
    #[test]
    fn tokens_of_all_cases() {
        let n = check_all(&cases(), |_| true, check_tokens);
        assert!(n > 1000);
    }

    // Not upstream's: a bulk file in the same format (the recorder's
    // thousands of mutations), when YAML3_PARSER_BULK names one.
    #[test]
    fn bulk_file_from_env() {
        let Ok(path) = std::env::var("YAML3_PARSER_BULK") else {
            return;
        };
        let text = std::fs::read_to_string(path).expect("bulk file");
        let bulk: Vec<Case> = serde_json::from_str(&text).expect("bulk json");
        let n = check_all(&bulk, |_| true, check_events);
        check_all(&bulk, |_| true, check_tokens);
        println!("bulk: {n} cases match");
    }

    // Not upstream's: the recordings reach every error message the
    // reader, scanner and parser can produce.
    #[test]
    fn recordings_cover_every_error_message() {
        let mut seen = std::collections::BTreeSet::new();
        for case in cases() {
            if let Some(Value::Array(err)) = &case.err
                && let (Some(Value::String(problem)), Some(Value::String(context))) =
                    (err.get(1), err.get(5))
            {
                seen.insert(format!("{problem} | {context}"));
                seen.insert(problem.clone());
            }
        }
        let messages = [
            // Reader.
            "invalid leading UTF-8 octet",
            "invalid trailing UTF-8 octet",
            "incomplete UTF-8 octet sequence",
            "invalid length of a UTF-8 sequence",
            "invalid Unicode character",
            "control characters are not allowed",
            "incomplete UTF-16 character",
            "unexpected low surrogate area",
            "incomplete UTF-16 surrogate pair",
            "expected low surrogate area",
            // Scanner.
            "could not find expected ':'",
            "exceeded max depth of 10000 | while increasing flow level",
            "exceeded max depth of 10000 | while increasing indent level",
            "block sequence entries are not allowed in this context",
            "mapping keys are not allowed in this context",
            "mapping values are not allowed in this context",
            "found character that cannot start any token",
            "could not find expected directive name",
            "found unexpected non-alphabetical character",
            "found unknown directive name",
            "did not find expected comment or line break | while scanning a directive",
            "did not find expected digit or '.' character",
            "did not find expected version number",
            "found extremely long version number",
            "did not find expected whitespace",
            "did not find expected whitespace or line break | while scanning a %TAG directive",
            "did not find expected whitespace or line break | while scanning a tag",
            "did not find expected alphabetic or numeric character | while scanning an anchor",
            "did not find expected alphabetic or numeric character | while scanning an alias",
            "did not find the expected '>'",
            "did not find expected '!' | while parsing a %TAG directive",
            "did not find expected tag URI | while parsing a %TAG directive",
            "did not find expected tag URI | while parsing a tag",
            "did not find URI escaped octet | while parsing a %TAG directive",
            "did not find URI escaped octet | while parsing a tag",
            "found an incorrect leading UTF-8 octet",
            "found an incorrect trailing UTF-8 octet",
            "found an indentation indicator equal to 0",
            "did not find expected comment or line break | while scanning a block scalar",
            "found a tab character where an indentation space is expected",
            "found unexpected document indicator",
            "found unexpected end of stream",
            "did not find expected hexdecimal number",
            "found invalid Unicode character escape code",
            "found unknown escape character",
            "found a tab character that violates indentation",
            // Parser.
            "did not find expected <document start>",
            "did not find expected node content | while parsing a block node",
            "did not find expected node content | while parsing a flow node",
            "did not find expected '-' indicator",
            "did not find expected key",
            "did not find expected ',' or ']'",
            "did not find expected ',' or '}'",
            "found duplicate %YAML directive",
            "found incompatible YAML document",
            "found duplicate %TAG directive",
            "found undefined tag handle",
        ];
        let missing: Vec<&str> = messages
            .iter()
            .copied()
            .filter(|m| !seen.contains(*m))
            .collect();
        assert!(missing.is_empty(), "no recording reaches {missing:?}");
    }

    /// The first error parsing `input`, as decode.go reports it.
    fn first_error(input: &[u8]) -> Option<String> {
        run_events(input)
            .1
            .map(|e| format!("yaml: {}", e.message()))
    }

    // Ported from yaml.v3 decode_test.go unmarshalErrorTests: the entries
    // whose error comes from the reader, scanner or parser (the others are
    // the decoder's).
    #[test]
    fn unmarshal_error_tests() {
        let exact: [(&[u8], &str); 8] = [
            (
                b"v: [A,",
                "yaml: line 1: did not find expected node content",
            ),
            (
                b"v:\n- [A,",
                "yaml: line 2: did not find expected node content",
            ),
            (
                b"a:\n- b: *,",
                "yaml: line 2: did not find expected alphabetic or numeric character",
            ),
            (
                b"value: -",
                "yaml: block sequence entries are not allowed in this context",
            ),
            (
                b"%TAG !%79! tag:yaml.org,2002:\n---\nv: !%79!int '1'",
                "yaml: did not find expected whitespace",
            ),
            (
                b"a: 1\nb: 2\nc 2\nd: 3\n",
                "yaml: line 3: could not find expected ':'",
            ),
            (b"#\n-\n{", "yaml: line 3: could not find expected ':'"),
            (b"0: [:!00 \xef", "yaml: incomplete UTF-8 octet sequence"),
        ];
        for (input, want) in exact {
            assert_eq!(first_error(input).as_deref(), Some(want));
        }
        // The regexp ".*could not find expected ':'".
        let got = first_error(b"a:\n  1:\nb\n  2:").expect("an error");
        assert!(got.ends_with("could not find expected ':'"), "{got}");
    }

    /// Parse to STREAM-END: the number of events, or decode.go's message
    /// for the first error.
    fn parse_all(input: &[u8]) -> Result<usize, String> {
        let mut parser = Parser::new(input);
        let mut n = 0;
        loop {
            match parser.parse() {
                Ok(ev) if ev.typ == EventType::StreamEnd => return Ok(n),
                Ok(_) => n += 1,
                Err(e) => return Err(format!("yaml: {}", e.message())),
            }
        }
    }

    // Ported from yaml.v3 limit_test.go limitTests, at the parser level:
    // the depth limits fail with yaml.v3's message and the large inputs
    // parse (the excessive-aliasing limit is the decoder's, so that input
    // parses here).
    #[test]
    fn limit_tests() {
        let depth = "yaml: exceeded max depth of 10000";
        let fail = [
            "[".repeat(1000 * 1024),
            format!("x: {}", "{".repeat(1000 * 1024)),
            "- ".repeat(1000 * 1024),
        ];
        for input in fail {
            assert_eq!(parse_all(input.as_bytes()), Err(depth.to_owned()));
        }
        let pass = [
            format!(
                "{{a: &a [{{a}}{}], b: &b [*a{}]}}",
                ",{a}".repeat(1000 * 1024 / 4 - 100),
                ",*a".repeat(99)
            ),
            format!("{}\n", "- ".repeat(1000)).repeat(1024 / 2),
            format!("a: &a [{{a}}{}]", ",{a}".repeat(1024 / 4 - 1)),
            format!("a: &a [{{a}}{}]", ",{a}".repeat(10 * 1024 / 4 - 1)),
            format!("a: &a [{{a}}{}]", ",{a}".repeat(100 * 1024 / 4 - 1)),
            format!("a: &a [{{a}}{}]", ",{a}".repeat(1000 * 1024 / 4 - 1)),
            format!(
                "{}1{}{}",
                "[".repeat(10000),
                ",1".repeat(1000 * 1024 / 2 - 20000 - 1),
                "]".repeat(10000)
            ),
            format!(
                "{{a,b:\n{} [1{}]{}",
                " {a,b:".repeat(10000 - 2),
                ",1".repeat(1000 * 1024 / 2 - 6 * 10000 - 1),
                "}".repeat(10000 - 1)
            ),
            format!("- {}{}\n", "[".repeat(10000), "]".repeat(10000)).repeat(1000 * 1024 / 20000),
        ];
        for input in pass {
            assert!(parse_all(input.as_bytes()).is_ok());
        }
    }

    // Not upstream's: after STREAM-END, and in the END state, parse
    // returns NO events.
    #[test]
    fn parse_after_stream_end_returns_no_events() {
        let mut parser = Parser::new(b"a: 1\n");
        let mut last = EventType::No;
        for _ in 0..10 {
            last = parser.parse().expect("no error").typ;
            if last == EventType::StreamEnd {
                break;
            }
        }
        assert_eq!(last, EventType::StreamEnd);
        for _ in 0..3 {
            assert_eq!(parser.parse().expect("no error").typ, EventType::No);
        }
    }

    // Not upstream's: after an error, parse returns the same error again.
    #[test]
    fn parse_after_error_repeats_it() {
        let mut parser = Parser::new(b"a: b: c\n");
        let first = loop {
            if let Err(e) = parser.parse() {
                break e;
            }
        };
        assert_eq!(
            first.problem,
            "mapping values are not allowed in this context"
        );
        assert_eq!(parser.parse(), Err(first.clone()));
        assert_eq!(parser.parse(), Err(first));
    }

    // Not upstream's: scan returns NO tokens after STREAM-END and repeats
    // its error.
    #[test]
    fn scan_after_end_and_error() {
        let mut parser = Parser::new(b"a");
        let types: Vec<TokenType> = (0..6)
            .map(|_| parser.scan().expect("no error").typ)
            .collect();
        assert_eq!(
            types,
            [
                TokenType::StreamStart,
                TokenType::Scalar,
                TokenType::StreamEnd,
                TokenType::No,
                TokenType::No,
                TokenType::No,
            ]
        );
        let mut parser = Parser::new(b"\"a");
        let first = loop {
            if let Err(e) = parser.scan() {
                break e;
            }
        };
        assert_eq!(first.problem, "found unexpected end of stream");
        assert_eq!(parser.scan(), Err(first));
    }

    // Not upstream's: an error's Debug output carries no input text.
    #[test]
    fn errors_carry_no_input_text() {
        let (_, err) = run_events(b"not-a-secret: [\"quoted-text");
        let text = format!("{:?}", err.expect("an error"));
        assert!(!text.contains("not-a-secret") && !text.contains("quoted-text"));
    }
}
