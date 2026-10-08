// Ported from gopkg.in/yaml.v3 v3.0.1 emitterc.go (flush, put, put_break,
// write, write_all, write_break, yaml_emitter_emit, the yaml_emitter_emit_*
// states, yaml_emitter_check_*, yaml_emitter_select_scalar_style,
// yaml_emitter_process_*, yaml_emitter_analyze_*, yaml_emitter_write_*),
// writerc.go (yaml_emitter_flush), apic.go (yaml_emitter_initialize,
// yaml_string_write_handler, yaml_emitter_set_output_string,
// yaml_emitter_set_encoding, yaml_emitter_set_canonical,
// yaml_emitter_set_indent, yaml_emitter_set_width, yaml_emitter_set_unicode,
// yaml_emitter_set_break) and yamlh.go (yaml_emitter_t,
// yaml_emitter_state_t) (MIT, from libyaml), the YAML library CLIProxyAPI
// v8.0.20 (MIT) reads and writes its config with.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/go-yaml/yaml
//
// Copyright (c) 2006-2010 Kirill Simonov
// Copyright (c) 2006-2011 Kirill Simonov
// Copyright (c) 2011-2019 Canonical Ltd
// Licensed under the MIT License; see licenses/go-yaml-LICENSE.

//! yaml.v3's emitter and writer: [`Emitter`] turns a stream of [`Event`]s
//! into YAML text in memory, byte for byte as `yaml_emitter_emit` does with
//! `yaml_emitter_set_output_string`.
//!
//! Events are queued until the emitter can look far enough ahead (one
//! event after DOCUMENT-START, two after SEQUENCE-START, three after
//! MAPPING-START), then each is analyzed (anchor, tag and scalar checks,
//! comments) and fed to the state machine, which writes into a small
//! working buffer that is flushed to the output when it fills, at the end
//! of each document and at the end of the stream, as yaml.v3's is.
//!
//! As in yaml.v3 v3.0.1, a UTF-16 stream encoding only makes the emitter
//! start the output with a UTF-8 byte order mark; the text itself is always
//! UTF-8 (its `writerc.go` writes the buffer out unconverted).
//!
//! Deviations from upstream:
//! - Indexing is panic-free. Where yaml.v3 would panic or loop forever on
//!   bytes that aren't UTF-8 (a byte that can't start a character, or a
//!   character cut off by the end of a value, tag or comment), the emitter
//!   returns an emitter error, `invalid UTF-8 sequence`. Bytes yaml.v3
//!   copies through without panicking (overlong forms, surrogates) are
//!   written as it writes them.
//! - Where yaml.v3 would panic on an empty state or indent stack (it can't
//!   happen through [`Emitter::emit`]), on a best indent of 0 set after
//!   STREAM-START, or on a line break setting of [`Break::Any`] set after
//!   STREAM-START, the emitter returns an emitter error.
//! - A scalar style of [`ScalarStyle::Any`] reaching the scalar writer
//!   (it can't: the style selection turns it into plain) gives the error
//!   `unknown scalar style` where yaml.v3 panics.
//! - Integer settings and counters are `i64` and saturate instead of
//!   wrapping at values far past any real output.
//! - Errors are returned as values. Once an error is returned, every later
//!   [`Emitter::emit`] and [`Emitter::flush`] returns it again, where
//!   yaml.v3 retries the queued event.
//! - The event being processed is taken off the queue first; the look-ahead
//!   checks read it and the queue behind it, as yaml.v3 reads its queue
//!   from `events_head`. The analysis takes the anchor, value and comments
//!   out of the event rather than pointing into it. Neither changes the
//!   output.
//! - `Emitter::set_encoding` returns an error where yaml.v3 panics
//!   ("must set the output encoding only once").
//! - The emitter adds [`Emitter::set_best_indent`] and
//!   [`Emitter::set_open_ended`] for the two fields yaml.v3's encoder sets
//!   directly (`best_indent` in `encoder.init`, `open_ended` in
//!   `encoder.finish`).

use std::collections::VecDeque;
use std::fmt;
use std::mem;

use super::chars::{
    OUTPUT_BUFFER_SIZE, at, is_alpha, is_ascii, is_blank, is_blankz, is_bom, is_break,
    is_printable, is_space, width,
};
use super::types::{
    Break, CollectionStyle, EmitterError, Encoding, Event, EventType, ScalarStyle, TagDirective,
    VersionDirective,
};

/// The initial capacity of the state stack (`initial_stack_size`).
const INITIAL_STACK_SIZE: usize = 16;
/// The initial capacity of the event queue (`initial_queue_size`).
const INITIAL_QUEUE_SIZE: usize = 16;

/// The problem reported where yaml.v3 would panic or loop on bytes that
/// aren't UTF-8.
const INVALID_UTF8: &str = "invalid UTF-8 sequence";

/// The problem reported where yaml.v3 would panic on an empty stack.
const EMPTY_STACK: &str = "invalid emitter state";

/// The tag directives every document has (`default_tag_directives`).
const DEFAULT_TAG_DIRECTIVES: [(&[u8], &[u8]); 2] = [(b"!", b"!"), (b"!!", b"tag:yaml.org,2002:")];

/// An emitter state (`yaml_emitter_state_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum EmitterState {
    /// Expect STREAM-START.
    #[default]
    StreamStart,
    /// Expect the first DOCUMENT-START or STREAM-END.
    FirstDocumentStart,
    /// Expect DOCUMENT-START or STREAM-END.
    DocumentStart,
    /// Expect the content of a document.
    DocumentContent,
    /// Expect DOCUMENT-END.
    DocumentEnd,
    /// Expect the first item of a flow sequence.
    FlowSequenceFirstItem,
    /// Expect the next item of a flow sequence, with the comma already
    /// written out.
    FlowSequenceTrailItem,
    /// Expect an item of a flow sequence.
    FlowSequenceItem,
    /// Expect the first key of a flow mapping.
    FlowMappingFirstKey,
    /// Expect the next key of a flow mapping, with the comma already
    /// written out.
    FlowMappingTrailKey,
    /// Expect a key of a flow mapping.
    FlowMappingKey,
    /// Expect a value for a simple key of a flow mapping.
    FlowMappingSimpleValue,
    /// Expect a value of a flow mapping.
    FlowMappingValue,
    /// Expect the first item of a block sequence.
    BlockSequenceFirstItem,
    /// Expect an item of a block sequence.
    BlockSequenceItem,
    /// Expect the first key of a block mapping.
    BlockMappingFirstKey,
    /// Expect the key of a block mapping.
    BlockMappingKey,
    /// Expect a value for a simple key of a block mapping.
    BlockMappingSimpleValue,
    /// Expect a value of a block mapping.
    BlockMappingValue,
    /// Expect nothing.
    End,
}

/// Anchor analysis (`anchor_data`).
#[derive(Clone, Default)]
struct AnchorData {
    /// The anchor value; empty for none (yaml.v3's nil: it only ever holds
    /// nil or a non-empty anchor).
    anchor: Vec<u8>,
    /// Is it an alias?
    alias: bool,
}

/// Tag analysis (`tag_data`).
#[derive(Clone, Default)]
struct TagData {
    /// The tag handle.
    handle: Vec<u8>,
    /// The tag suffix.
    suffix: Vec<u8>,
}

/// Scalar analysis (`scalar_data`).
#[derive(Clone, Default)]
struct ScalarData {
    /// The scalar value.
    value: Vec<u8>,
    /// Does the scalar contain line breaks?
    multiline: bool,
    /// Can the scalar be expressed in the flow plain style?
    flow_plain_allowed: bool,
    /// Can the scalar be expressed in the block plain style?
    block_plain_allowed: bool,
    /// Can the scalar be expressed in the single quoted style?
    single_quoted_allowed: bool,
    /// Can the scalar be expressed in the literal or folded styles?
    block_allowed: bool,
    /// The output style.
    style: ScalarStyle,
}

/// yaml_emitter_t, writing to an in-memory buffer.
pub(crate) struct Emitter {
    /// The error that stopped the emitter (`error` and `problem`).
    error: Option<EmitterError>,

    /// String output data (`output_buffer`): everything flushed so far.
    output: Vec<u8>,
    /// The working buffer (`buffer`); its length is `buffer_pos`.
    buffer: Vec<u8>,

    /// The stream encoding.
    encoding: Encoding,

    /// If the output is in the canonical style?
    canonical: bool,
    /// The number of indentation spaces.
    best_indent: i64,
    /// The preferred width of the output lines.
    best_width: i64,
    /// Allow unescaped non-ASCII characters?
    unicode: bool,
    /// The preferred line break.
    line_break: Break,

    /// The current emitter state.
    state: EmitterState,
    /// The stack of states.
    states: Vec<EmitterState>,

    /// The event queue, from `events_head` on.
    events: VecDeque<Event>,

    /// The stack of indentation levels.
    indents: Vec<i64>,

    /// The list of tag directives.
    tag_directives: Vec<TagDirective>,

    /// The current indentation level.
    indent: i64,

    /// The current flow level.
    flow_level: i64,

    /// Is it the document root context?
    root_context: bool,
    /// Is it a sequence context?
    sequence_context: bool,
    /// Is it a mapping context?
    mapping_context: bool,
    /// Is it a simple mapping key context?
    simple_key_context: bool,

    /// The current line.
    line: i64,
    /// The current column.
    column: i64,
    /// If the last character was a whitespace?
    whitespace: bool,
    /// If the last character was an indentation character (' ', '-', '?',
    /// ':')?
    indention: bool,
    /// If an explicit document end is required?
    open_ended: bool,

    /// Is there's an empty line above?
    space_above: bool,
    /// The indent used to write the foot comment above, or -1 if none.
    foot_indent: i64,

    /// Anchor analysis.
    anchor_data: AnchorData,
    /// Tag analysis.
    tag_data: TagData,
    /// Scalar analysis.
    scalar_data: ScalarData,

    /// The pending head comment.
    head_comment: Vec<u8>,
    /// The pending line comment.
    line_comment: Vec<u8>,
    /// The pending foot comment.
    foot_comment: Vec<u8>,
    /// The pending tail comment.
    tail_comment: Vec<u8>,

    /// A line comment given for a mapping key, kept for its value.
    key_line_comment: Vec<u8>,
}

impl fmt::Debug for Emitter {
    // The output and comments may hold secrets; show only the position.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Emitter")
            .field("state", &self.state)
            .field("line", &self.line)
            .field("column", &self.column)
            .finish_non_exhaustive()
    }
}

impl Default for Emitter {
    fn default() -> Self {
        Self::new()
    }
}

/// The error for bytes yaml.v3 can't walk through as UTF-8.
fn invalid_utf8() -> EmitterError {
    EmitterError::emitter(INVALID_UTF8)
}

/// The error for an empty state or indent stack.
fn empty_stack() -> EmitterError {
    EmitterError::emitter(EMPTY_STACK)
}

/// yaml_emitter_set_emitter_error: an emitter error.
fn set_emitter_error(problem: &str) -> Result<(), EmitterError> {
    Err(EmitterError::emitter(problem))
}

impl Emitter {
    /// yaml_emitter_initialize, then yaml_emitter_set_output_string.
    pub(crate) fn new() -> Self {
        Self {
            error: None,
            output: Vec::new(),
            buffer: Vec::with_capacity(OUTPUT_BUFFER_SIZE),
            encoding: Encoding::Any,
            canonical: false,
            best_indent: 0,
            best_width: -1,
            unicode: false,
            line_break: Break::Any,
            state: EmitterState::StreamStart,
            states: Vec::with_capacity(INITIAL_STACK_SIZE),
            events: VecDeque::with_capacity(INITIAL_QUEUE_SIZE),
            indents: Vec::new(),
            tag_directives: Vec::new(),
            indent: 0,
            flow_level: 0,
            root_context: false,
            sequence_context: false,
            mapping_context: false,
            simple_key_context: false,
            line: 0,
            column: 0,
            whitespace: false,
            indention: false,
            open_ended: false,
            space_above: false,
            foot_indent: 0,
            anchor_data: AnchorData::default(),
            tag_data: TagData::default(),
            scalar_data: ScalarData::default(),
            head_comment: Vec::new(),
            line_comment: Vec::new(),
            foot_comment: Vec::new(),
            tail_comment: Vec::new(),
            key_line_comment: Vec::new(),
        }
    }

    /// yaml_emitter_set_encoding. yaml.v3 panics if the encoding was
    /// already set; this returns an error.
    #[cfg(test)]
    pub(crate) fn set_encoding(&mut self, encoding: Encoding) -> Result<(), EmitterError> {
        if self.encoding != Encoding::Any {
            return set_emitter_error("must set the output encoding only once");
        }
        self.encoding = encoding;
        Ok(())
    }

    /// yaml_emitter_set_canonical.
    #[cfg(test)]
    pub(crate) fn set_canonical(&mut self, canonical: bool) {
        self.canonical = canonical;
    }

    /// yaml_emitter_set_indent (values outside 2..=9 become 2).
    #[cfg(test)]
    pub(crate) fn set_indent(&mut self, indent: i32) {
        let indent = if !(2..=9).contains(&indent) {
            2
        } else {
            indent
        };
        self.best_indent = i64::from(indent);
    }

    /// Sets best_indent directly, as yaml.v3's encoder.init does
    /// (`e.emitter.best_indent = e.indent`), without set_indent's clamp.
    pub(crate) fn set_best_indent(&mut self, indent: i32) {
        self.best_indent = i64::from(indent);
    }

    /// yaml_emitter_set_width.
    #[cfg(test)]
    pub(crate) fn set_width(&mut self, width: i32) {
        let width = if width < 0 { -1 } else { width };
        self.best_width = i64::from(width);
    }

    /// yaml_emitter_set_unicode.
    pub(crate) fn set_unicode(&mut self, unicode: bool) {
        self.unicode = unicode;
    }

    /// yaml_emitter_set_break.
    #[cfg(test)]
    pub(crate) fn set_break(&mut self, line_break: Break) {
        self.line_break = line_break;
    }

    /// Sets open_ended directly, as yaml.v3's encoder.finish does
    /// (`e.emitter.open_ended = false`) before STREAM-END, so that a
    /// stream ending in a plain root scalar gets no `...` line.
    pub(crate) fn set_open_ended(&mut self, open_ended: bool) {
        self.open_ended = open_ended;
    }

    /// yaml_emitter_emit.
    pub(crate) fn emit(&mut self, event: Event) -> Result<(), EmitterError> {
        if let Some(err) = &self.error {
            return Err(err.clone());
        }
        let result = self.emit_queued(event);
        if let Err(err) = &result {
            self.error = Some(err.clone());
        }
        result
    }

    /// yaml_emitter_flush.
    #[cfg(test)]
    pub(crate) fn flush(&mut self) -> Result<(), EmitterError> {
        if let Some(err) = &self.error {
            return Err(err.clone());
        }
        let result = self.flush_buffer();
        if let Err(err) = &result {
            self.error = Some(err.clone());
        }
        result
    }

    /// The bytes written so far (flushed output).
    #[cfg(test)]
    pub(crate) fn output(&self) -> &[u8] {
        &self.output
    }

    /// The bytes written, consuming the emitter.
    pub(crate) fn into_output(self) -> Vec<u8> {
        self.output
    }

    // ---- writerc.go and apic.go: the output ----

    /// yaml_emitter_flush: write the working buffer out through
    /// yaml_string_write_handler, which appends it to the output and can't
    /// fail (so yaml_emitter_set_writer_error is never reached).
    fn flush_buffer(&mut self) -> Result<(), EmitterError> {
        // Check if the buffer is empty.
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.output.extend_from_slice(&self.buffer);
        self.buffer.clear();
        Ok(())
    }

    // ---- emitterc.go: buffer helpers ----

    /// flush: flush the buffer if needed.
    fn flush_if_needed(&mut self) -> Result<(), EmitterError> {
        if self.buffer.len().saturating_add(5) >= OUTPUT_BUFFER_SIZE {
            return self.flush_buffer();
        }
        Ok(())
    }

    /// put: put a character to the output buffer.
    fn put(&mut self, value: u8) -> Result<(), EmitterError> {
        self.flush_if_needed()?;
        self.buffer.push(value);
        self.column = self.column.saturating_add(1);
        Ok(())
    }

    /// put_break: put a line break to the output buffer.
    fn put_break(&mut self) -> Result<(), EmitterError> {
        self.flush_if_needed()?;
        match self.line_break {
            Break::Cr => self.buffer.push(b'\r'),
            Break::Ln => self.buffer.push(b'\n'),
            Break::CrLn => self.buffer.extend_from_slice(b"\r\n"),
            Break::Any => return set_emitter_error("unknown line break setting"),
        }
        if self.column == 0 {
            self.space_above = true;
        }
        self.column = 0;
        self.line = self.line.saturating_add(1);
        // [Go] Do this here and below and drop from everywhere else (see
        // commented lines).
        self.indention = true;
        Ok(())
    }

    /// write: copy a character from a string into buffer.
    fn write(&mut self, s: &[u8], i: &mut usize) -> Result<(), EmitterError> {
        self.flush_if_needed()?;
        let w = width(at(s, *i));
        if w == 0 {
            // yaml.v3 panics: "unknown character width".
            return Err(invalid_utf8());
        }
        let ch = i
            .checked_add(w)
            .and_then(|end| s.get(*i..end))
            .ok_or_else(invalid_utf8)?;
        self.buffer.extend_from_slice(ch);
        self.column = self.column.saturating_add(1);
        *i = i.saturating_add(w);
        Ok(())
    }

    /// write_all: write a whole string into buffer.
    fn write_all(&mut self, s: &[u8]) -> Result<(), EmitterError> {
        let mut i = 0;
        while i < s.len() {
            self.write(s, &mut i)?;
        }
        Ok(())
    }

    /// write_break: copy a line break character from a string into buffer.
    fn write_break(&mut self, s: &[u8], i: &mut usize) -> Result<(), EmitterError> {
        if at(s, *i) == b'\n' {
            self.put_break()?;
            *i = i.saturating_add(1);
        } else {
            self.write(s, i)?;
            if self.column == 0 {
                self.space_above = true;
            }
            self.column = 0;
            self.line = self.line.saturating_add(1);
            // [Go] Do this here and above and drop from everywhere else
            // (see commented lines).
            self.indention = true;
        }
        Ok(())
    }

    // ---- emitterc.go: the event queue ----

    /// yaml_emitter_emit, after the error check: queue the event and
    /// process every event that has enough look-ahead.
    fn emit_queued(&mut self, event: Event) -> Result<(), EmitterError> {
        self.events.push_back(event);
        while !self.need_more_events() {
            let Some(mut event) = self.events.pop_front() else {
                break;
            };
            self.analyze_event(&mut event)?;
            self.state_machine(&event)?;
        }
        Ok(())
    }

    /// yaml_emitter_need_more_events: check if we need to accumulate more
    /// events before emitting.
    ///
    /// We accumulate extra
    ///  - 1 event for DOCUMENT-START
    ///  - 2 events for SEQUENCE-START
    ///  - 3 events for MAPPING-START
    fn need_more_events(&self) -> bool {
        let Some(head) = self.events.front() else {
            return true;
        };
        let accumulate = match head.typ {
            EventType::DocumentStart => 1,
            EventType::SequenceStart => 2,
            EventType::MappingStart => 3,
            _ => return false,
        };
        if self.events.len() > accumulate {
            return false;
        }
        let mut level: i64 = 0;
        for event in &self.events {
            match event.typ {
                EventType::StreamStart
                | EventType::DocumentStart
                | EventType::SequenceStart
                | EventType::MappingStart => level = level.saturating_add(1),
                EventType::StreamEnd
                | EventType::DocumentEnd
                | EventType::SequenceEnd
                | EventType::MappingEnd => level = level.saturating_sub(1),
                _ => {}
            }
            if level == 0 {
                return false;
            }
        }
        true
    }

    /// yaml_emitter_append_tag_directive: append a directive to the
    /// directives stack.
    fn append_tag_directive(
        &mut self,
        handle: &[u8],
        prefix: &[u8],
        allow_duplicates: bool,
    ) -> Result<(), EmitterError> {
        if self
            .tag_directives
            .iter()
            .any(|td| td.handle.as_slice() == handle)
        {
            if allow_duplicates {
                return Ok(());
            }
            return set_emitter_error("duplicate %TAG directive");
        }
        self.tag_directives.push(TagDirective {
            handle: handle.to_vec(),
            prefix: prefix.to_vec(),
        });
        Ok(())
    }

    /// yaml_emitter_increase_indent: increase the indentation level.
    fn increase_indent(&mut self, flow: bool, indentless: bool) -> Result<(), EmitterError> {
        self.indents.push(self.indent);
        if self.indent < 0 {
            self.indent = if flow { self.best_indent } else { 0 };
        } else if !indentless {
            // [Go] This was changed so that indentations are more regular.
            let top = *self.states.last().ok_or_else(empty_stack)?;
            if top == EmitterState::BlockSequenceItem {
                // The first indent inside a sequence will just skip the "- "
                // indicator.
                self.indent = self.indent.saturating_add(2);
            } else {
                // Everything else aligns to the chosen indentation.
                let levels = self
                    .indent
                    .saturating_add(self.best_indent)
                    .checked_div(self.best_indent)
                    .ok_or_else(empty_stack)?;
                self.indent = self.best_indent.saturating_mul(levels);
            }
        }
        Ok(())
    }

    /// Pop the state stack into the current state.
    fn pop_state(&mut self) -> Result<(), EmitterError> {
        self.state = self.states.pop().ok_or_else(empty_stack)?;
        Ok(())
    }

    /// Pop the indent stack into the current indent.
    fn pop_indent(&mut self) -> Result<(), EmitterError> {
        self.indent = self.indents.pop().ok_or_else(empty_stack)?;
        Ok(())
    }

    /// Whether a line, foot or tail comment is pending.
    fn has_trailing_comments(&self) -> bool {
        !self.line_comment.is_empty()
            || !self.foot_comment.is_empty()
            || !self.tail_comment.is_empty()
    }

    // ---- emitterc.go: the states ----

    /// yaml_emitter_state_machine: state dispatcher.
    fn state_machine(&mut self, event: &Event) -> Result<(), EmitterError> {
        match self.state {
            EmitterState::StreamStart => self.emit_stream_start(event),
            EmitterState::FirstDocumentStart => self.emit_document_start(event, true),
            EmitterState::DocumentStart => self.emit_document_start(event, false),
            EmitterState::DocumentContent => self.emit_document_content(event),
            EmitterState::DocumentEnd => self.emit_document_end(event),
            EmitterState::FlowSequenceFirstItem => self.emit_flow_sequence_item(event, true, false),
            EmitterState::FlowSequenceTrailItem => self.emit_flow_sequence_item(event, false, true),
            EmitterState::FlowSequenceItem => self.emit_flow_sequence_item(event, false, false),
            EmitterState::FlowMappingFirstKey => self.emit_flow_mapping_key(event, true, false),
            EmitterState::FlowMappingTrailKey => self.emit_flow_mapping_key(event, false, true),
            EmitterState::FlowMappingKey => self.emit_flow_mapping_key(event, false, false),
            EmitterState::FlowMappingSimpleValue => self.emit_flow_mapping_value(event, true),
            EmitterState::FlowMappingValue => self.emit_flow_mapping_value(event, false),
            EmitterState::BlockSequenceFirstItem => self.emit_block_sequence_item(event, true),
            EmitterState::BlockSequenceItem => self.emit_block_sequence_item(event, false),
            EmitterState::BlockMappingFirstKey => self.emit_block_mapping_key(event, true),
            EmitterState::BlockMappingKey => self.emit_block_mapping_key(event, false),
            EmitterState::BlockMappingSimpleValue => self.emit_block_mapping_value(event, true),
            EmitterState::BlockMappingValue => self.emit_block_mapping_value(event, false),
            EmitterState::End => set_emitter_error("expected nothing after STREAM-END"),
        }
    }

    /// yaml_emitter_emit_stream_start: expect STREAM-START.
    fn emit_stream_start(&mut self, event: &Event) -> Result<(), EmitterError> {
        if event.typ != EventType::StreamStart {
            return set_emitter_error("expected STREAM-START");
        }
        if self.encoding == Encoding::Any {
            self.encoding = event.encoding;
            if self.encoding == Encoding::Any {
                self.encoding = Encoding::Utf8;
            }
        }
        if self.best_indent < 2 || self.best_indent > 9 {
            self.best_indent = 2;
        }
        if self.best_width >= 0 && self.best_width <= self.best_indent.saturating_mul(2) {
            self.best_width = 80;
        }
        if self.best_width < 0 {
            self.best_width = (1 << 31) - 1;
        }
        if self.line_break == Break::Any {
            self.line_break = Break::Ln;
        }

        self.indent = -1;
        self.line = 0;
        self.column = 0;
        self.whitespace = true;
        self.indention = true;
        self.space_above = true;
        self.foot_indent = -1;

        if self.encoding != Encoding::Utf8 {
            self.write_bom()?;
        }
        self.state = EmitterState::FirstDocumentStart;
        Ok(())
    }

    /// yaml_emitter_emit_document_start: expect DOCUMENT-START or
    /// STREAM-END.
    fn emit_document_start(&mut self, event: &Event, first: bool) -> Result<(), EmitterError> {
        if event.typ == EventType::DocumentStart {
            if let Some(version_directive) = &event.version_directive {
                analyze_version_directive(version_directive)?;
            }

            for tag_directive in &event.tag_directives {
                analyze_tag_directive(tag_directive)?;
                self.append_tag_directive(&tag_directive.handle, &tag_directive.prefix, false)?;
            }

            for (handle, prefix) in DEFAULT_TAG_DIRECTIVES {
                self.append_tag_directive(handle, prefix, true)?;
            }

            let mut implicit = event.implicit;
            if !first || self.canonical {
                implicit = false;
            }

            if self.open_ended
                && (event.version_directive.is_some() || !event.tag_directives.is_empty())
            {
                self.write_indicator(b"...", true, false, false)?;
                self.write_indent()?;
            }

            if event.version_directive.is_some() {
                implicit = false;
                self.write_indicator(b"%YAML", true, false, false)?;
                self.write_indicator(b"1.1", true, false, false)?;
                self.write_indent()?;
            }

            if !event.tag_directives.is_empty() {
                implicit = false;
                for tag_directive in &event.tag_directives {
                    self.write_indicator(b"%TAG", true, false, false)?;
                    self.write_tag_handle(&tag_directive.handle)?;
                    self.write_tag_content(&tag_directive.prefix, true)?;
                    self.write_indent()?;
                }
            }

            if check_empty_document() {
                implicit = false;
            }
            if !implicit {
                self.write_indent()?;
                self.write_indicator(b"---", true, false, false)?;
                // [Go] `if emitter.canonical || true`.
                self.write_indent()?;
            }

            if !self.head_comment.is_empty() {
                self.process_head_comment()?;
                self.put_break()?;
            }

            self.state = EmitterState::DocumentContent;
            return Ok(());
        }

        if event.typ == EventType::StreamEnd {
            if self.open_ended {
                self.write_indicator(b"...", true, false, false)?;
                self.write_indent()?;
            }
            self.flush_buffer()?;
            self.state = EmitterState::End;
            return Ok(());
        }

        set_emitter_error("expected DOCUMENT-START or STREAM-END")
    }

    /// yaml_emitter_emit_document_content: expect the root node.
    fn emit_document_content(&mut self, event: &Event) -> Result<(), EmitterError> {
        self.states.push(EmitterState::DocumentEnd);
        self.process_head_comment()?;
        self.emit_node(event, true, false, false, false)?;
        self.process_line_comment()?;
        self.process_foot_comment()
    }

    /// yaml_emitter_emit_document_end: expect DOCUMENT-END.
    fn emit_document_end(&mut self, event: &Event) -> Result<(), EmitterError> {
        if event.typ != EventType::DocumentEnd {
            return set_emitter_error("expected DOCUMENT-END");
        }
        // [Go] Force document foot separation.
        self.foot_indent = 0;
        self.process_foot_comment()?;
        self.foot_indent = -1;
        self.write_indent()?;
        if !event.implicit {
            // [Go] Allocate the slice elsewhere.
            self.write_indicator(b"...", true, false, false)?;
            self.write_indent()?;
        }
        self.flush_buffer()?;
        self.state = EmitterState::DocumentStart;
        self.tag_directives.clear();
        Ok(())
    }

    /// yaml_emitter_emit_flow_sequence_item: expect a flow item node.
    fn emit_flow_sequence_item(
        &mut self,
        event: &Event,
        first: bool,
        trail: bool,
    ) -> Result<(), EmitterError> {
        if first {
            self.write_indicator(b"[", true, true, false)?;
            self.increase_indent(true, false)?;
            self.flow_level = self.flow_level.saturating_add(1);
        }

        if event.typ == EventType::SequenceEnd {
            if self.canonical && !first && !trail {
                self.write_indicator(b",", false, false, false)?;
            }
            self.flow_level = self.flow_level.saturating_sub(1);
            self.pop_indent()?;
            if self.column == 0 || self.canonical && !first {
                self.write_indent()?;
            }
            self.write_indicator(b"]", false, false, false)?;
            self.process_line_comment()?;
            self.process_foot_comment()?;
            return self.pop_state();
        }

        if !first && !trail {
            self.write_indicator(b",", false, false, false)?;
        }

        self.process_head_comment()?;
        if self.column == 0 {
            self.write_indent()?;
        }

        if self.canonical || self.column > self.best_width {
            self.write_indent()?;
        }
        if self.has_trailing_comments() {
            self.states.push(EmitterState::FlowSequenceTrailItem);
        } else {
            self.states.push(EmitterState::FlowSequenceItem);
        }
        self.emit_node(event, false, true, false, false)?;
        if self.has_trailing_comments() {
            self.write_indicator(b",", false, false, false)?;
        }
        self.process_line_comment()?;
        self.process_foot_comment()
    }

    /// yaml_emitter_emit_flow_mapping_key: expect a flow key node.
    fn emit_flow_mapping_key(
        &mut self,
        event: &Event,
        first: bool,
        trail: bool,
    ) -> Result<(), EmitterError> {
        if first {
            self.write_indicator(b"{", true, true, false)?;
            self.increase_indent(true, false)?;
            self.flow_level = self.flow_level.saturating_add(1);
        }

        if event.typ == EventType::MappingEnd {
            if (self.canonical
                || !self.head_comment.is_empty()
                || !self.foot_comment.is_empty()
                || !self.tail_comment.is_empty())
                && !first
                && !trail
            {
                self.write_indicator(b",", false, false, false)?;
            }
            self.process_head_comment()?;
            self.flow_level = self.flow_level.saturating_sub(1);
            self.pop_indent()?;
            if self.canonical && !first {
                self.write_indent()?;
            }
            self.write_indicator(b"}", false, false, false)?;
            self.process_line_comment()?;
            self.process_foot_comment()?;
            return self.pop_state();
        }

        if !first && !trail {
            self.write_indicator(b",", false, false, false)?;
        }

        self.process_head_comment()?;

        if self.column == 0 {
            self.write_indent()?;
        }

        if self.canonical || self.column > self.best_width {
            self.write_indent()?;
        }

        if !self.canonical && self.check_simple_key(event) {
            self.states.push(EmitterState::FlowMappingSimpleValue);
            return self.emit_node(event, false, false, true, true);
        }
        self.write_indicator(b"?", true, false, false)?;
        self.states.push(EmitterState::FlowMappingValue);
        self.emit_node(event, false, false, true, false)
    }

    /// yaml_emitter_emit_flow_mapping_value: expect a flow value node.
    fn emit_flow_mapping_value(&mut self, event: &Event, simple: bool) -> Result<(), EmitterError> {
        if simple {
            self.write_indicator(b":", false, false, false)?;
        } else {
            if self.canonical || self.column > self.best_width {
                self.write_indent()?;
            }
            self.write_indicator(b":", true, false, false)?;
        }
        if self.has_trailing_comments() {
            self.states.push(EmitterState::FlowMappingTrailKey);
        } else {
            self.states.push(EmitterState::FlowMappingKey);
        }
        self.emit_node(event, false, false, true, false)?;
        if self.has_trailing_comments() {
            self.write_indicator(b",", false, false, false)?;
        }
        self.process_line_comment()?;
        self.process_foot_comment()
    }

    /// yaml_emitter_emit_block_sequence_item: expect a block item node.
    fn emit_block_sequence_item(&mut self, event: &Event, first: bool) -> Result<(), EmitterError> {
        if first {
            self.increase_indent(false, false)?;
        }
        if event.typ == EventType::SequenceEnd {
            self.pop_indent()?;
            return self.pop_state();
        }
        self.process_head_comment()?;
        self.write_indent()?;
        self.write_indicator(b"-", true, false, true)?;
        self.states.push(EmitterState::BlockSequenceItem);
        self.emit_node(event, false, true, false, false)?;
        self.process_line_comment()?;
        self.process_foot_comment()
    }

    /// yaml_emitter_emit_block_mapping_key: expect a block key node.
    fn emit_block_mapping_key(&mut self, event: &Event, first: bool) -> Result<(), EmitterError> {
        if first {
            self.increase_indent(false, false)?;
        }
        self.process_head_comment()?;
        if event.typ == EventType::MappingEnd {
            self.pop_indent()?;
            return self.pop_state();
        }
        self.write_indent()?;
        if !self.line_comment.is_empty() {
            // [Go] A line comment was provided for the key. That's unusual
            //      as the scanner associates line comments with the value.
            //      Either way, save the line comment and render it
            //      appropriately later.
            self.key_line_comment = mem::take(&mut self.line_comment);
        }
        if self.check_simple_key(event) {
            self.states.push(EmitterState::BlockMappingSimpleValue);
            return self.emit_node(event, false, false, true, true);
        }
        self.write_indicator(b"?", true, false, true)?;
        self.states.push(EmitterState::BlockMappingValue);
        self.emit_node(event, false, false, true, false)
    }

    /// yaml_emitter_emit_block_mapping_value: expect a block value node.
    fn emit_block_mapping_value(
        &mut self,
        event: &Event,
        simple: bool,
    ) -> Result<(), EmitterError> {
        if simple {
            self.write_indicator(b":", false, false, false)?;
        } else {
            self.write_indent()?;
            self.write_indicator(b":", true, false, true)?;
        }
        if !self.key_line_comment.is_empty() {
            // [Go] Line comments are generally associated with the value,
            //      but when there's no value on the same line as a mapping
            //      key they end up attached to the key itself.
            if event.typ == EventType::Scalar {
                if self.line_comment.is_empty() {
                    // A scalar is coming and it has no line comments by
                    // itself yet, so just let it handle the line comment as
                    // usual. If it has a line comment, we can't have both so
                    // the one from the key is lost.
                    self.line_comment = mem::take(&mut self.key_line_comment);
                }
            } else if event.collection_style != CollectionStyle::Flow
                && (event.typ == EventType::MappingStart || event.typ == EventType::SequenceStart)
            {
                // An indented block follows, so write the comment right now.
                mem::swap(&mut self.line_comment, &mut self.key_line_comment);
                self.process_line_comment()?;
                mem::swap(&mut self.line_comment, &mut self.key_line_comment);
            }
        }
        self.states.push(EmitterState::BlockMappingKey);
        self.emit_node(event, false, false, true, false)?;
        self.process_line_comment()?;
        self.process_foot_comment()
    }

    /// yaml_emitter_emit_node: expect a node.
    fn emit_node(
        &mut self,
        event: &Event,
        root: bool,
        sequence: bool,
        mapping: bool,
        simple_key: bool,
    ) -> Result<(), EmitterError> {
        self.root_context = root;
        self.sequence_context = sequence;
        self.mapping_context = mapping;
        self.simple_key_context = simple_key;

        match event.typ {
            EventType::Alias => self.emit_alias(),
            EventType::Scalar => self.emit_scalar(event),
            EventType::SequenceStart => self.emit_sequence_start(event),
            EventType::MappingStart => self.emit_mapping_start(event),
            other => set_emitter_error(&format!(
                "expected SCALAR, SEQUENCE-START, MAPPING-START, or ALIAS, but got {other}"
            )),
        }
    }

    /// yaml_emitter_emit_alias: expect ALIAS.
    fn emit_alias(&mut self) -> Result<(), EmitterError> {
        self.process_anchor()?;
        self.pop_state()
    }

    /// yaml_emitter_emit_scalar: expect SCALAR.
    fn emit_scalar(&mut self, event: &Event) -> Result<(), EmitterError> {
        self.select_scalar_style(event)?;
        self.process_anchor()?;
        self.process_tag()?;
        self.increase_indent(true, false)?;
        self.process_scalar()?;
        self.pop_indent()?;
        self.pop_state()
    }

    /// yaml_emitter_emit_sequence_start: expect SEQUENCE-START.
    fn emit_sequence_start(&mut self, event: &Event) -> Result<(), EmitterError> {
        self.process_anchor()?;
        self.process_tag()?;
        if self.flow_level > 0
            || self.canonical
            || event.collection_style == CollectionStyle::Flow
            || self.check_empty_sequence(event)
        {
            self.state = EmitterState::FlowSequenceFirstItem;
        } else {
            self.state = EmitterState::BlockSequenceFirstItem;
        }
        Ok(())
    }

    /// yaml_emitter_emit_mapping_start: expect MAPPING-START.
    fn emit_mapping_start(&mut self, event: &Event) -> Result<(), EmitterError> {
        self.process_anchor()?;
        self.process_tag()?;
        if self.flow_level > 0
            || self.canonical
            || event.collection_style == CollectionStyle::Flow
            || self.check_empty_mapping(event)
        {
            self.state = EmitterState::FlowMappingFirstKey;
        } else {
            self.state = EmitterState::BlockMappingFirstKey;
        }
        Ok(())
    }

    // ---- emitterc.go: checks ----

    /// yaml_emitter_check_empty_sequence: check if the next events
    /// represent an empty sequence. `event` is the queue head.
    fn check_empty_sequence(&self, event: &Event) -> bool {
        event.typ == EventType::SequenceStart
            && self.events.front().map(|e| e.typ) == Some(EventType::SequenceEnd)
    }

    /// yaml_emitter_check_empty_mapping: check if the next events represent
    /// an empty mapping. `event` is the queue head.
    fn check_empty_mapping(&self, event: &Event) -> bool {
        event.typ == EventType::MappingStart
            && self.events.front().map(|e| e.typ) == Some(EventType::MappingEnd)
    }

    /// yaml_emitter_check_simple_key: check if the next node can be
    /// expressed as a simple key. `event` is the queue head.
    fn check_simple_key(&self, event: &Event) -> bool {
        let length = match event.typ {
            EventType::Alias => self.anchor_data.anchor.len(),
            EventType::Scalar => {
                if self.scalar_data.multiline {
                    return false;
                }
                self.anchor_data
                    .anchor
                    .len()
                    .saturating_add(self.tag_data.handle.len())
                    .saturating_add(self.tag_data.suffix.len())
                    .saturating_add(self.scalar_data.value.len())
            }
            EventType::SequenceStart => {
                if !self.check_empty_sequence(event) {
                    return false;
                }
                self.anchor_data
                    .anchor
                    .len()
                    .saturating_add(self.tag_data.handle.len())
                    .saturating_add(self.tag_data.suffix.len())
            }
            EventType::MappingStart => {
                if !self.check_empty_mapping(event) {
                    return false;
                }
                self.anchor_data
                    .anchor
                    .len()
                    .saturating_add(self.tag_data.handle.len())
                    .saturating_add(self.tag_data.suffix.len())
            }
            _ => return false,
        };
        length <= 128
    }

    /// yaml_emitter_select_scalar_style: determine an acceptable scalar
    /// style.
    fn select_scalar_style(&mut self, event: &Event) -> Result<(), EmitterError> {
        let no_tag = self.tag_data.handle.is_empty() && self.tag_data.suffix.is_empty();
        if no_tag && !event.implicit && !event.quoted_implicit {
            return set_emitter_error("neither tag nor implicit flags are specified");
        }

        let mut style = event.scalar_style;
        if style == ScalarStyle::Any {
            style = ScalarStyle::Plain;
        }
        if self.canonical {
            style = ScalarStyle::DoubleQuoted;
        }
        if self.simple_key_context && self.scalar_data.multiline {
            style = ScalarStyle::DoubleQuoted;
        }

        if style == ScalarStyle::Plain {
            if self.flow_level > 0 && !self.scalar_data.flow_plain_allowed
                || self.flow_level == 0 && !self.scalar_data.block_plain_allowed
            {
                style = ScalarStyle::SingleQuoted;
            }
            if self.scalar_data.value.is_empty() && (self.flow_level > 0 || self.simple_key_context)
            {
                style = ScalarStyle::SingleQuoted;
            }
            if no_tag && !event.implicit {
                style = ScalarStyle::SingleQuoted;
            }
        }
        if style == ScalarStyle::SingleQuoted && !self.scalar_data.single_quoted_allowed {
            style = ScalarStyle::DoubleQuoted;
        }
        if (style == ScalarStyle::Literal || style == ScalarStyle::Folded)
            && (!self.scalar_data.block_allowed || self.flow_level > 0 || self.simple_key_context)
        {
            style = ScalarStyle::DoubleQuoted;
        }

        if no_tag && !event.quoted_implicit && style != ScalarStyle::Plain {
            self.tag_data.handle = b"!".to_vec();
        }
        self.scalar_data.style = style;
        Ok(())
    }

    // ---- emitterc.go: processing ----

    /// yaml_emitter_process_anchor: write an anchor.
    fn process_anchor(&mut self) -> Result<(), EmitterError> {
        if self.anchor_data.anchor.is_empty() {
            return Ok(());
        }
        let c: &[u8] = if self.anchor_data.alias { b"*" } else { b"&" };
        self.write_indicator(c, true, false, false)?;
        let anchor = mem::take(&mut self.anchor_data.anchor);
        let result = self.write_anchor(&anchor);
        self.anchor_data.anchor = anchor;
        result
    }

    /// yaml_emitter_process_tag: write a tag.
    fn process_tag(&mut self) -> Result<(), EmitterError> {
        if self.tag_data.handle.is_empty() && self.tag_data.suffix.is_empty() {
            return Ok(());
        }
        let tag_data = mem::take(&mut self.tag_data);
        let result = self.write_tag(&tag_data);
        self.tag_data = tag_data;
        result
    }

    /// The writing half of yaml_emitter_process_tag.
    fn write_tag(&mut self, tag_data: &TagData) -> Result<(), EmitterError> {
        if !tag_data.handle.is_empty() {
            self.write_tag_handle(&tag_data.handle)?;
            if !tag_data.suffix.is_empty() {
                self.write_tag_content(&tag_data.suffix, false)?;
            }
        } else {
            // [Go] Allocate these slices elsewhere.
            self.write_indicator(b"!<", true, false, false)?;
            self.write_tag_content(&tag_data.suffix, false)?;
            self.write_indicator(b">", false, false, false)?;
        }
        Ok(())
    }

    /// yaml_emitter_process_scalar: write a scalar.
    fn process_scalar(&mut self) -> Result<(), EmitterError> {
        let value = mem::take(&mut self.scalar_data.value);
        let allow_breaks = !self.simple_key_context;
        let result = match self.scalar_data.style {
            // yaml.v3 panics here; select_scalar_style never leaves Any.
            ScalarStyle::Any => set_emitter_error("unknown scalar style"),
            ScalarStyle::Plain => self.write_plain_scalar(&value, allow_breaks),
            ScalarStyle::SingleQuoted => self.write_single_quoted_scalar(&value, allow_breaks),
            ScalarStyle::DoubleQuoted => self.write_double_quoted_scalar(&value, allow_breaks),
            ScalarStyle::Literal => self.write_literal_scalar(&value),
            ScalarStyle::Folded => self.write_folded_scalar(&value),
        };
        self.scalar_data.value = value;
        result
    }

    /// yaml_emitter_process_head_comment: write a head comment.
    fn process_head_comment(&mut self) -> Result<(), EmitterError> {
        if !self.tail_comment.is_empty() {
            self.write_indent()?;
            let tail_comment = mem::take(&mut self.tail_comment);
            self.write_comment(&tail_comment)?;
            self.foot_indent = self.indent;
            if self.foot_indent < 0 {
                self.foot_indent = 0;
            }
        }

        if self.head_comment.is_empty() {
            return Ok(());
        }
        self.write_indent()?;
        let head_comment = mem::take(&mut self.head_comment);
        self.write_comment(&head_comment)
    }

    /// yaml_emitter_process_line_comment: write a line comment.
    fn process_line_comment(&mut self) -> Result<(), EmitterError> {
        if self.line_comment.is_empty() {
            return Ok(());
        }
        if !self.whitespace {
            self.put(b' ')?;
        }
        let line_comment = mem::take(&mut self.line_comment);
        self.write_comment(&line_comment)
    }

    /// yaml_emitter_process_foot_comment: write a foot comment.
    fn process_foot_comment(&mut self) -> Result<(), EmitterError> {
        if self.foot_comment.is_empty() {
            return Ok(());
        }
        self.write_indent()?;
        let foot_comment = mem::take(&mut self.foot_comment);
        self.write_comment(&foot_comment)?;
        self.foot_indent = self.indent;
        if self.foot_indent < 0 {
            self.foot_indent = 0;
        }
        Ok(())
    }

    // ---- emitterc.go: analysis ----

    /// yaml_emitter_analyze_anchor: check if an anchor is valid.
    fn analyze_anchor(&mut self, anchor: Vec<u8>, alias: bool) -> Result<(), EmitterError> {
        if anchor.is_empty() {
            let problem = if alias {
                "alias value must not be empty"
            } else {
                "anchor value must not be empty"
            };
            return set_emitter_error(problem);
        }
        let mut i = 0;
        while i < anchor.len() {
            if !is_alpha(&anchor, i) {
                let problem = if alias {
                    "alias value must contain alphanumerical characters only"
                } else {
                    "anchor value must contain alphanumerical characters only"
                };
                return set_emitter_error(problem);
            }
            // An alphanumerical character is one byte wide.
            i = i.saturating_add(width(at(&anchor, i)));
        }
        self.anchor_data.anchor = anchor;
        self.anchor_data.alias = alias;
        Ok(())
    }

    /// yaml_emitter_analyze_tag: check if a tag is valid.
    fn analyze_tag(&mut self, tag: &[u8]) -> Result<(), EmitterError> {
        if tag.is_empty() {
            return set_emitter_error("tag value must not be empty");
        }
        let matched = self.tag_directives.iter().find_map(|tag_directive| {
            tag.strip_prefix(tag_directive.prefix.as_slice())
                .map(|suffix| (tag_directive.handle.clone(), suffix.to_vec()))
        });
        if let Some((handle, suffix)) = matched {
            self.tag_data.handle = handle;
            self.tag_data.suffix = suffix;
            return Ok(());
        }
        self.tag_data.suffix = tag.to_vec();
        Ok(())
    }

    /// yaml_emitter_analyze_scalar: check if a scalar is valid.
    fn analyze_scalar(&mut self, value: Vec<u8>) -> Result<(), EmitterError> {
        let mut block_indicators = false;
        let mut flow_indicators = false;
        let mut line_breaks = false;
        let mut special_characters = false;
        let mut tab_characters = false;

        let mut leading_space = false;
        let mut leading_break = false;
        let mut trailing_space = false;
        let mut trailing_break = false;
        let mut break_space = false;
        let mut space_break = false;

        let mut previous_space = false;
        let mut previous_break = false;

        if value.is_empty() {
            self.scalar_data.value = value;
            self.scalar_data.multiline = false;
            self.scalar_data.flow_plain_allowed = false;
            self.scalar_data.block_plain_allowed = true;
            self.scalar_data.single_quoted_allowed = true;
            self.scalar_data.block_allowed = false;
            return Ok(());
        }

        if value.starts_with(b"---") || value.starts_with(b"...") {
            block_indicators = true;
            flow_indicators = true;
        }

        let mut preceded_by_whitespace = true;
        let mut i = 0;
        while i < value.len() {
            let c = at(&value, i);
            let w = width(c);
            if w == 0 {
                // yaml.v3 loops forever here.
                return Err(invalid_utf8());
            }
            let next = i.saturating_add(w);
            let followed_by_whitespace = next >= value.len() || is_blank(&value, next);

            if i == 0 {
                match c {
                    b'#' | b',' | b'[' | b']' | b'{' | b'}' | b'&' | b'*' | b'!' | b'|' | b'>'
                    | b'\'' | b'"' | b'%' | b'@' | b'`' => {
                        flow_indicators = true;
                        block_indicators = true;
                    }
                    b'?' | b':' => {
                        flow_indicators = true;
                        if followed_by_whitespace {
                            block_indicators = true;
                        }
                    }
                    b'-' if followed_by_whitespace => {
                        flow_indicators = true;
                        block_indicators = true;
                    }
                    _ => {}
                }
            } else {
                match c {
                    b',' | b'?' | b'[' | b']' | b'{' | b'}' => {
                        flow_indicators = true;
                    }
                    b':' => {
                        flow_indicators = true;
                        if followed_by_whitespace {
                            block_indicators = true;
                        }
                    }
                    b'#' if preceded_by_whitespace => {
                        flow_indicators = true;
                        block_indicators = true;
                    }
                    _ => {}
                }
            }

            if c == b'\t' {
                tab_characters = true;
            } else if !is_printable(&value, i) || !is_ascii(&value, i) && !self.unicode {
                special_characters = true;
            }
            if is_space(&value, i) {
                if i == 0 {
                    leading_space = true;
                }
                if next == value.len() {
                    trailing_space = true;
                }
                if previous_break {
                    break_space = true;
                }
                previous_space = true;
                previous_break = false;
            } else if is_break(&value, i) {
                line_breaks = true;
                if i == 0 {
                    leading_break = true;
                }
                if next == value.len() {
                    trailing_break = true;
                }
                if previous_space {
                    space_break = true;
                }
                previous_space = false;
                previous_break = true;
            } else {
                previous_space = false;
                previous_break = false;
            }

            // [Go]: Why 'z'? Couldn't be the end of the string as that's the
            // loop condition.
            preceded_by_whitespace = is_blankz(&value, i);
            i = next;
        }

        let data = &mut self.scalar_data;
        data.value = value;
        data.multiline = line_breaks;
        data.flow_plain_allowed = true;
        data.block_plain_allowed = true;
        data.single_quoted_allowed = true;
        data.block_allowed = true;

        if leading_space || leading_break || trailing_space || trailing_break {
            data.flow_plain_allowed = false;
            data.block_plain_allowed = false;
        }
        if trailing_space {
            data.block_allowed = false;
        }
        if break_space {
            data.flow_plain_allowed = false;
            data.block_plain_allowed = false;
            data.single_quoted_allowed = false;
        }
        if space_break || tab_characters || special_characters {
            data.flow_plain_allowed = false;
            data.block_plain_allowed = false;
            data.single_quoted_allowed = false;
        }
        if space_break || special_characters {
            data.block_allowed = false;
        }
        if line_breaks {
            data.flow_plain_allowed = false;
            data.block_plain_allowed = false;
        }
        if flow_indicators {
            data.flow_plain_allowed = false;
        }
        if block_indicators {
            data.block_plain_allowed = false;
        }
        Ok(())
    }

    /// yaml_emitter_analyze_event: check if the event data is valid. The
    /// comments, anchor and value move from the event into the emitter.
    fn analyze_event(&mut self, event: &mut Event) -> Result<(), EmitterError> {
        self.anchor_data.anchor = Vec::new();
        self.tag_data.handle = Vec::new();
        self.tag_data.suffix = Vec::new();
        self.scalar_data.value = Vec::new();

        if !event.head_comment.is_empty() {
            self.head_comment = mem::take(&mut event.head_comment);
        }
        if !event.line_comment.is_empty() {
            self.line_comment = mem::take(&mut event.line_comment);
        }
        if !event.foot_comment.is_empty() {
            self.foot_comment = mem::take(&mut event.foot_comment);
        }
        if !event.tail_comment.is_empty() {
            self.tail_comment = mem::take(&mut event.tail_comment);
        }

        match event.typ {
            EventType::Alias => {
                self.analyze_anchor(mem::take(&mut event.anchor), true)?;
            }
            EventType::Scalar => {
                if !event.anchor.is_empty() {
                    self.analyze_anchor(mem::take(&mut event.anchor), false)?;
                }
                if !event.tag.is_empty()
                    && (self.canonical || (!event.implicit && !event.quoted_implicit))
                {
                    self.analyze_tag(&event.tag)?;
                }
                self.analyze_scalar(mem::take(&mut event.value))?;
            }
            EventType::SequenceStart | EventType::MappingStart => {
                if !event.anchor.is_empty() {
                    self.analyze_anchor(mem::take(&mut event.anchor), false)?;
                }
                if !event.tag.is_empty() && (self.canonical || !event.implicit) {
                    self.analyze_tag(&event.tag)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ---- emitterc.go: writing ----

    /// yaml_emitter_write_bom: write the BOM character.
    fn write_bom(&mut self) -> Result<(), EmitterError> {
        self.flush_if_needed()?;
        self.buffer.extend_from_slice(b"\xEF\xBB\xBF");
        Ok(())
    }

    /// yaml_emitter_write_indent.
    fn write_indent(&mut self) -> Result<(), EmitterError> {
        let indent = self.indent.max(0);
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.put_break()?;
        }
        if self.foot_indent == indent {
            self.put_break()?;
        }
        while self.column < indent {
            self.put(b' ')?;
        }
        self.whitespace = true;
        //emitter.indention = true
        self.space_above = false;
        self.foot_indent = -1;
        Ok(())
    }

    /// yaml_emitter_write_indicator.
    fn write_indicator(
        &mut self,
        indicator: &[u8],
        need_whitespace: bool,
        is_whitespace: bool,
        is_indention: bool,
    ) -> Result<(), EmitterError> {
        if need_whitespace && !self.whitespace {
            self.put(b' ')?;
        }
        self.write_all(indicator)?;
        self.whitespace = is_whitespace;
        self.indention = self.indention && is_indention;
        self.open_ended = false;
        Ok(())
    }

    /// yaml_emitter_write_anchor.
    fn write_anchor(&mut self, value: &[u8]) -> Result<(), EmitterError> {
        self.write_all(value)?;
        self.whitespace = false;
        self.indention = false;
        Ok(())
    }

    /// yaml_emitter_write_tag_handle.
    fn write_tag_handle(&mut self, value: &[u8]) -> Result<(), EmitterError> {
        if !self.whitespace {
            self.put(b' ')?;
        }
        self.write_all(value)?;
        self.whitespace = false;
        self.indention = false;
        Ok(())
    }

    /// yaml_emitter_write_tag_content.
    fn write_tag_content(
        &mut self,
        value: &[u8],
        need_whitespace: bool,
    ) -> Result<(), EmitterError> {
        if need_whitespace && !self.whitespace {
            self.put(b' ')?;
        }
        let mut i = 0;
        while i < value.len() {
            let must_write = match at(value, i) {
                b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'_'
                | b'.' | b'~' | b'*' | b'\'' | b'(' | b')' | b'[' | b']' => true,
                _ => is_alpha(value, i),
            };
            if must_write {
                self.write(value, &mut i)?;
            } else {
                let w = width(at(value, i));
                if w == 0 {
                    // yaml.v3 loops forever here.
                    return Err(invalid_utf8());
                }
                for _ in 0..w {
                    let octet = *value.get(i).ok_or_else(invalid_utf8)?;
                    i = i.saturating_add(1);
                    self.put(b'%')?;
                    self.put(hex_digit(octet >> 4))?;
                    self.put(hex_digit(octet & 0x0f))?;
                }
            }
        }
        self.whitespace = false;
        self.indention = false;
        Ok(())
    }

    /// yaml_emitter_write_plain_scalar.
    fn write_plain_scalar(&mut self, value: &[u8], allow_breaks: bool) -> Result<(), EmitterError> {
        if !value.is_empty() && !self.whitespace {
            self.put(b' ')?;
        }

        let mut spaces = false;
        let mut breaks = false;
        let mut i = 0;
        while i < value.len() {
            if is_space(value, i) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && !is_space(value, i.saturating_add(1))
                {
                    self.write_indent()?;
                    i = i.saturating_add(1);
                } else {
                    self.write(value, &mut i)?;
                }
                spaces = true;
            } else if is_break(value, i) {
                if !breaks && at(value, i) == b'\n' {
                    self.put_break()?;
                }
                self.write_break(value, &mut i)?;
                //emitter.indention = true
                breaks = true;
            } else {
                if breaks {
                    self.write_indent()?;
                }
                self.write(value, &mut i)?;
                self.indention = false;
                spaces = false;
                breaks = false;
            }
        }

        if !value.is_empty() {
            self.whitespace = false;
        }
        self.indention = false;
        if self.root_context {
            self.open_ended = true;
        }

        Ok(())
    }

    /// yaml_emitter_write_single_quoted_scalar.
    fn write_single_quoted_scalar(
        &mut self,
        value: &[u8],
        allow_breaks: bool,
    ) -> Result<(), EmitterError> {
        self.write_indicator(b"'", true, false, false)?;

        let mut spaces = false;
        let mut breaks = false;
        let mut i = 0;
        while i < value.len() {
            if is_space(value, i) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && i > 0
                    && i < value.len().saturating_sub(1)
                    && !is_space(value, i.saturating_add(1))
                {
                    self.write_indent()?;
                    i = i.saturating_add(1);
                } else {
                    self.write(value, &mut i)?;
                }
                spaces = true;
            } else if is_break(value, i) {
                if !breaks && at(value, i) == b'\n' {
                    self.put_break()?;
                }
                self.write_break(value, &mut i)?;
                //emitter.indention = true
                breaks = true;
            } else {
                if breaks {
                    self.write_indent()?;
                }
                if at(value, i) == b'\'' {
                    self.put(b'\'')?;
                }
                self.write(value, &mut i)?;
                self.indention = false;
                spaces = false;
                breaks = false;
            }
        }
        self.write_indicator(b"'", false, false, false)?;
        self.whitespace = false;
        self.indention = false;
        Ok(())
    }

    /// yaml_emitter_write_double_quoted_scalar.
    fn write_double_quoted_scalar(
        &mut self,
        value: &[u8],
        allow_breaks: bool,
    ) -> Result<(), EmitterError> {
        let mut spaces = false;
        self.write_indicator(b"\"", true, false, false)?;

        let mut i = 0;
        while i < value.len() {
            let octet = at(value, i);
            if !is_printable(value, i)
                || (!self.unicode && !is_ascii(value, i))
                || is_bom(value, i)
                || is_break(value, i)
                || octet == b'"'
                || octet == b'\\'
            {
                let (w, mut v): (usize, u32) = if octet & 0x80 == 0x00 {
                    (1, u32::from(octet & 0x7F))
                } else if octet & 0xE0 == 0xC0 {
                    (2, u32::from(octet & 0x1F))
                } else if octet & 0xF0 == 0xE0 {
                    (3, u32::from(octet & 0x0F))
                } else if octet & 0xF8 == 0xF0 {
                    (4, u32::from(octet & 0x07))
                } else {
                    // yaml.v3 writes `\0` here forever.
                    return Err(invalid_utf8());
                };
                for k in 1..w {
                    let next = *i
                        .checked_add(k)
                        .and_then(|j| value.get(j))
                        .ok_or_else(invalid_utf8)?;
                    v = (v << 6) | u32::from(next & 0x3F);
                }
                i = i.saturating_add(w);

                self.put(b'\\')?;

                match v {
                    0x00 => self.put(b'0')?,
                    0x07 => self.put(b'a')?,
                    0x08 => self.put(b'b')?,
                    0x09 => self.put(b't')?,
                    0x0A => self.put(b'n')?,
                    0x0b => self.put(b'v')?,
                    0x0c => self.put(b'f')?,
                    0x0d => self.put(b'r')?,
                    0x1b => self.put(b'e')?,
                    0x22 => self.put(b'"')?,
                    0x5c => self.put(b'\\')?,
                    0x85 => self.put(b'N')?,
                    0xA0 => self.put(b'_')?,
                    0x2028 => self.put(b'L')?,
                    0x2029 => self.put(b'P')?,
                    _ => {
                        let digits: u32 = if v <= 0xFF {
                            self.put(b'x')?;
                            2
                        } else if v <= 0xFFFF {
                            self.put(b'u')?;
                            4
                        } else {
                            self.put(b'U')?;
                            8
                        };
                        for k in (0..digits).rev() {
                            let shift = k.saturating_mul(4);
                            let digit = (v.checked_shr(shift).unwrap_or(0) & 0x0F) as u8;
                            self.put(hex_digit(digit))?;
                        }
                    }
                }
                spaces = false;
            } else if is_space(value, i) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && i > 0
                    && i < value.len().saturating_sub(1)
                {
                    self.write_indent()?;
                    if is_space(value, i.saturating_add(1)) {
                        self.put(b'\\')?;
                    }
                    i = i.saturating_add(1);
                } else {
                    self.write(value, &mut i)?;
                }
                spaces = true;
            } else {
                self.write(value, &mut i)?;
                spaces = false;
            }
        }
        self.write_indicator(b"\"", false, false, false)?;
        self.whitespace = false;
        self.indention = false;
        Ok(())
    }

    /// yaml_emitter_write_block_scalar_hints.
    fn write_block_scalar_hints(&mut self, value: &[u8]) -> Result<(), EmitterError> {
        if is_space(value, 0) || is_break(value, 0) {
            // Go's `'0' + byte(emitter.best_indent)`, truncating and
            // wrapping as Go does.
            let indent_hint = [b'0'.wrapping_add(self.best_indent as u8)];
            self.write_indicator(&indent_hint, false, false, false)?;
        }

        self.open_ended = false;

        let mut chomp_hint = 0u8;
        if value.is_empty() {
            chomp_hint = b'-';
        } else {
            let mut i = value.len().saturating_sub(1);
            while at(value, i) & 0xC0 == 0x80 {
                i = i.checked_sub(1).ok_or_else(invalid_utf8)?;
            }
            if !is_break(value, i) {
                chomp_hint = b'-';
            } else if i == 0 {
                chomp_hint = b'+';
                self.open_ended = true;
            } else {
                i = i.saturating_sub(1);
                while at(value, i) & 0xC0 == 0x80 {
                    i = i.checked_sub(1).ok_or_else(invalid_utf8)?;
                }
                if is_break(value, i) {
                    chomp_hint = b'+';
                    self.open_ended = true;
                }
            }
        }
        if chomp_hint != 0 {
            self.write_indicator(&[chomp_hint], false, false, false)?;
        }
        Ok(())
    }

    /// yaml_emitter_write_literal_scalar.
    fn write_literal_scalar(&mut self, value: &[u8]) -> Result<(), EmitterError> {
        self.write_indicator(b"|", true, false, false)?;
        self.write_block_scalar_hints(value)?;
        self.process_line_comment()?;
        //emitter.indention = true
        self.whitespace = true;
        let mut breaks = true;
        let mut i = 0;
        while i < value.len() {
            if is_break(value, i) {
                self.write_break(value, &mut i)?;
                //emitter.indention = true
                breaks = true;
            } else {
                if breaks {
                    self.write_indent()?;
                }
                self.write(value, &mut i)?;
                self.indention = false;
                breaks = false;
            }
        }

        Ok(())
    }

    /// yaml_emitter_write_folded_scalar.
    fn write_folded_scalar(&mut self, value: &[u8]) -> Result<(), EmitterError> {
        self.write_indicator(b">", true, false, false)?;
        self.write_block_scalar_hints(value)?;
        self.process_line_comment()?;

        //emitter.indention = true
        self.whitespace = true;

        let mut breaks = true;
        let mut leading_spaces = true;
        let mut i = 0;
        while i < value.len() {
            if is_break(value, i) {
                if !breaks && !leading_spaces && at(value, i) == b'\n' {
                    // yaml.v3 scans from the start of the value here, not
                    // from i.
                    let mut k = 0usize;
                    while is_break(value, k) {
                        k = k.saturating_add(width(at(value, k)));
                    }
                    if !is_blankz(value, k) {
                        self.put_break()?;
                    }
                }
                self.write_break(value, &mut i)?;
                //emitter.indention = true
                breaks = true;
            } else {
                if breaks {
                    self.write_indent()?;
                    leading_spaces = is_blank(value, i);
                }
                if !breaks
                    && is_space(value, i)
                    && !is_space(value, i.saturating_add(1))
                    && self.column > self.best_width
                {
                    self.write_indent()?;
                    i = i.saturating_add(1);
                } else {
                    self.write(value, &mut i)?;
                }
                self.indention = false;
                breaks = false;
            }
        }
        Ok(())
    }

    /// yaml_emitter_write_comment.
    fn write_comment(&mut self, comment: &[u8]) -> Result<(), EmitterError> {
        let mut breaks = false;
        let mut pound = false;
        let mut i = 0;
        while i < comment.len() {
            if is_break(comment, i) {
                self.write_break(comment, &mut i)?;
                //emitter.indention = true
                breaks = true;
                pound = false;
            } else {
                if breaks {
                    self.write_indent()?;
                }
                if !pound {
                    if at(comment, i) != b'#' {
                        self.put(b'#')?;
                        self.put(b' ')?;
                    }
                    pound = true;
                }
                self.write(comment, &mut i)?;
                self.indention = false;
                breaks = false;
            }
        }
        if !breaks {
            self.put_break()?;
        }

        self.whitespace = true;
        //emitter.indention = true
        Ok(())
    }
}

/// yaml_emitter_check_empty_document: check if the document content is an
/// empty scalar. yaml.v3's always says no (`// [Go] Huh?`).
fn check_empty_document() -> bool {
    false
}

/// yaml_emitter_analyze_version_directive: check if a %YAML directive is
/// valid.
fn analyze_version_directive(version_directive: &VersionDirective) -> Result<(), EmitterError> {
    if version_directive.major != 1 || version_directive.minor != 1 {
        return set_emitter_error("incompatible %YAML directive");
    }
    Ok(())
}

/// yaml_emitter_analyze_tag_directive: check if a %TAG directive is valid.
fn analyze_tag_directive(tag_directive: &TagDirective) -> Result<(), EmitterError> {
    let handle = &tag_directive.handle;
    let prefix = &tag_directive.prefix;
    let Some(&first) = handle.first() else {
        return set_emitter_error("tag handle must not be empty");
    };
    if first != b'!' {
        return set_emitter_error("tag handle must start with '!'");
    }
    if handle.last() != Some(&b'!') {
        return set_emitter_error("tag handle must end with '!'");
    }
    let mut i = 1;
    while i < handle.len().saturating_sub(1) {
        if !is_alpha(handle, i) {
            return set_emitter_error("tag handle must contain alphanumerical characters only");
        }
        // An alphanumerical character is one byte wide.
        i = i.saturating_add(width(at(handle, i)));
    }
    if prefix.is_empty() {
        return set_emitter_error("tag prefix must not be empty");
    }
    Ok(())
}

/// The uppercase hex digit for a value below 16, as yaml.v3 writes in tag
/// escapes and `\x`, `\u` and `\U` escapes.
fn hex_digit(c: u8) -> u8 {
    if c < 10 {
        c.wrapping_add(b'0')
    } else {
        c.wrapping_add(b'A' - 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::yaml3::types::ErrorType;
    use serde::Deserialize;

    /// An event as the test data stores it, with yaml.v3's numeric styles
    /// and encodings.
    #[derive(Deserialize)]
    struct JsonEvent {
        /// `+STR`, `-STR`, `+DOC`, `-DOC`, `+MAP`, `-MAP`, `+SEQ`, `-SEQ`,
        /// `=VAL`, `=ALI`, `TAIL` or `NONE`.
        t: String,
        /// The value.
        #[serde(default)]
        v: String,
        /// The anchor (or alias).
        #[serde(default)]
        a: String,
        /// The tag.
        #[serde(default)]
        g: String,
        /// The head comment.
        #[serde(default)]
        h: String,
        /// The line comment.
        #[serde(default)]
        l: String,
        /// The foot comment.
        #[serde(default)]
        f: String,
        /// The tail comment.
        #[serde(default)]
        tl: String,
        /// implicit.
        #[serde(default)]
        i: bool,
        /// quoted_implicit.
        #[serde(default)]
        q: bool,
        /// yaml.v3's numeric style.
        #[serde(default)]
        s: u8,
        /// yaml.v3's numeric encoding.
        #[serde(default)]
        e: u8,
        /// The version directive.
        #[serde(default)]
        ver: Option<[i8; 2]>,
        /// The tag directives, handle and prefix.
        #[serde(default)]
        tags: Vec<[String; 2]>,
    }

    /// Emitter settings, applied as the probe's `emit` mode applies them.
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct JsonSettings {
        /// Sets best_indent directly; 0 leaves it unset.
        indent: i32,
        /// yaml_emitter_set_unicode.
        unicode: bool,
        /// yaml_emitter_set_canonical.
        canonical: bool,
        /// yaml_emitter_set_break, numbered as yaml.v3 does.
        #[serde(rename = "break")]
        line_break: u8,
        /// Whether to call yaml_emitter_set_width.
        set_width: bool,
        /// The width to set.
        width: i32,
        /// Clears open_ended before STREAM-END, as encoder.finish does.
        finish: bool,
    }

    /// One run of a case's events and what yaml.v3 gave.
    #[derive(Deserialize)]
    struct JsonRun {
        /// The settings.
        #[serde(default)]
        settings: JsonSettings,
        /// The output.
        output: String,
        /// The problem, empty for none.
        #[serde(default)]
        error: String,
        /// The number of events accepted.
        accepted: usize,
    }

    /// An event stream and its runs.
    #[derive(Deserialize)]
    struct JsonCase {
        /// Names the case in failures.
        name: String,
        /// The events.
        events: Vec<JsonEvent>,
        /// Each run and its answers.
        runs: Vec<JsonRun>,
    }

    /// The event a test event stands for.
    fn event(j: &JsonEvent) -> Event {
        let typ = match j.t.as_str() {
            "+STR" => EventType::StreamStart,
            "-STR" => EventType::StreamEnd,
            "+DOC" => EventType::DocumentStart,
            "-DOC" => EventType::DocumentEnd,
            "+MAP" => EventType::MappingStart,
            "-MAP" => EventType::MappingEnd,
            "+SEQ" => EventType::SequenceStart,
            "-SEQ" => EventType::SequenceEnd,
            "=VAL" => EventType::Scalar,
            "=ALI" => EventType::Alias,
            "TAIL" => EventType::TailComment,
            "NONE" => EventType::No,
            other => panic!("unknown event type {other}"),
        };
        let scalar_style = match j.s {
            _ if typ != EventType::Scalar => ScalarStyle::Any,
            0 => ScalarStyle::Any,
            2 => ScalarStyle::Plain,
            4 => ScalarStyle::SingleQuoted,
            8 => ScalarStyle::DoubleQuoted,
            16 => ScalarStyle::Literal,
            32 => ScalarStyle::Folded,
            other => panic!("unknown scalar style {other}"),
        };
        let collection_style = match j.s {
            1 => CollectionStyle::Block,
            2 => CollectionStyle::Flow,
            _ => CollectionStyle::Any,
        };
        Event {
            typ,
            encoding: match j.e {
                1 => Encoding::Utf8,
                2 => Encoding::Utf16Le,
                3 => Encoding::Utf16Be,
                _ => Encoding::Any,
            },
            version_directive: j
                .ver
                .map(|[major, minor]| VersionDirective { major, minor }),
            tag_directives: j
                .tags
                .iter()
                .map(|[handle, prefix]| TagDirective {
                    handle: handle.as_bytes().to_vec(),
                    prefix: prefix.as_bytes().to_vec(),
                })
                .collect(),
            head_comment: j.h.as_bytes().to_vec(),
            line_comment: j.l.as_bytes().to_vec(),
            foot_comment: j.f.as_bytes().to_vec(),
            tail_comment: j.tl.as_bytes().to_vec(),
            anchor: j.a.as_bytes().to_vec(),
            tag: j.g.as_bytes().to_vec(),
            value: j.v.as_bytes().to_vec(),
            implicit: j.i,
            quoted_implicit: j.q,
            scalar_style,
            collection_style,
            ..Event::default()
        }
    }

    /// Runs events through a new emitter as the probe's `emit` mode does,
    /// giving the output, the problem (empty for none) and the number of
    /// events accepted.
    fn run(settings: &JsonSettings, events: &[JsonEvent]) -> (String, String, usize) {
        let mut emitter = Emitter::new();
        emitter.set_unicode(settings.unicode);
        emitter.set_canonical(settings.canonical);
        emitter.set_break(match settings.line_break {
            1 => Break::Cr,
            2 => Break::Ln,
            3 => Break::CrLn,
            _ => Break::Any,
        });
        if settings.set_width {
            emitter.set_width(settings.width);
        }
        if settings.indent != 0 {
            emitter.set_best_indent(settings.indent);
        }
        let text = |emitter: &Emitter| String::from_utf8_lossy(emitter.output()).into_owned();
        for (n, j) in events.iter().enumerate() {
            let ev = event(j);
            if settings.finish && ev.typ == EventType::StreamEnd {
                emitter.set_open_ended(false);
            }
            if let Err(err) = emitter.emit(ev) {
                assert_eq!(err.kind, ErrorType::Emitter);
                return (text(&emitter), err.problem, n);
            }
        }
        if let Err(err) = emitter.flush() {
            assert_eq!(err.kind, ErrorType::Emitter);
            return (text(&emitter), err.problem, events.len());
        }
        (text(&emitter), String::new(), events.len())
    }

    /// Checks every run of every case, naming the cases that differ, and
    /// returns the number of runs.
    fn check_cases(json: &str) -> usize {
        let cases: Vec<JsonCase> = serde_json::from_str(json).expect("test data");
        let mut failures = Vec::new();
        let mut runs = 0;
        for case in &cases {
            for (r, want) in case.runs.iter().enumerate() {
                runs += 1;
                let got = run(&want.settings, &case.events);
                let want = (want.output.clone(), want.error.clone(), want.accepted);
                if got != want {
                    failures.push(format!(
                        "{} (run {r}):\n  got  {got:?}\n  want {want:?}",
                        case.name
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {runs} runs differ:\n{}",
            failures.len(),
            failures
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        runs
    }

    // Ported from yaml.v3 node_test.go TestNodeRoundtrip (the encode half,
    // SetIndent(2)) and TestNodeZeroEncodeDecode, and encode_test.go
    // TestMarshal, TestSetIndent and TestEncoderMultipleDocuments: the
    // events yaml.v3's encoder passed to the emitter for each, recorded
    // with the probe, give the output the test expects.
    #[test]
    fn upstream_encoder_tests() {
        let runs = check_cases(include_str!("../testdata/yaml3/emitter_upstream.json"));
        assert!(runs > 100, "{runs} runs");
    }

    // Not upstream's: Go's answers recorded with the yaml.v3 probe, for the
    // events yaml.v3's encoder produces for yaml.Node trees of its test
    // inputs with SetIndent(2) and the default indent 4 (the output is the
    // roundtrip probe's).
    #[test]
    fn recorded_encoder_cases() {
        let runs = check_cases(include_str!("../testdata/yaml3/emitter_encoder.json"));
        assert!(runs > 200, "{runs} runs");
    }

    // Not upstream's: Go's answers recorded with the yaml.v3 probe, for the
    // parser's events over yaml.v3's test inputs with each setting, and for
    // synthetic streams: scalars in every style and context, comments,
    // empty collections, directives, documents, errors and random streams.
    #[test]
    fn recorded_cases() {
        let runs = check_cases(include_str!("../testdata/yaml3/emitter_cases.json"));
        assert!(runs > 600, "{runs} runs");
    }

    // Not upstream's: more cases recorded with the yaml.v3 probe, read from
    // the file EMITTER_CASES names, for checking large sets during
    // development.
    #[test]
    #[ignore]
    fn recorded_cases_from_env() {
        let path = std::env::var("EMITTER_CASES").expect("EMITTER_CASES");
        let json = std::fs::read_to_string(path).expect("read cases");
        let runs = check_cases(&json);
        eprintln!("{runs} runs match");
    }

    /// Emits the events, returning the first error.
    fn emit_all(emitter: &mut Emitter, events: Vec<Event>) -> Result<(), EmitterError> {
        for ev in events {
            emitter.emit(ev)?;
        }
        emitter.flush()
    }

    /// A stream with one document holding one scalar.
    fn scalar_stream(value: &[u8], tag: &[u8], style: ScalarStyle) -> Vec<Event> {
        vec![
            Event::stream_start(Encoding::Utf8),
            Event::document_start(None, Vec::new(), true),
            Event::scalar(Vec::new(), tag.to_vec(), value.to_vec(), true, true, style),
            Event::document_end(true),
            Event::stream_end(),
        ]
    }

    // Not upstream's: bytes that aren't UTF-8 give an error where yaml.v3
    // would loop forever or index out of range.
    #[test]
    fn invalid_utf8_is_an_error() {
        let styles = [
            ScalarStyle::Plain,
            ScalarStyle::SingleQuoted,
            ScalarStyle::DoubleQuoted,
            ScalarStyle::Literal,
            ScalarStyle::Folded,
        ];
        for value in [&b"a\xFFb"[..], b"a\xE2\x82", b"\xC3"] {
            for style in styles {
                let mut emitter = Emitter::new();
                let err = emit_all(&mut emitter, scalar_stream(value, b"", style)).unwrap_err();
                assert_eq!(err.problem, INVALID_UTF8, "{style:?}");
            }
            let mut events = scalar_stream(b"x", value, ScalarStyle::Plain);
            events[2].implicit = false;
            events[2].quoted_implicit = false;
            let mut emitter = Emitter::new();
            let err = emit_all(&mut emitter, events).unwrap_err();
            assert_eq!(err.problem, INVALID_UTF8);
        }
        let mut events = scalar_stream(b"x", b"", ScalarStyle::Plain);
        events[2].line_comment = b"# a\xE2".to_vec();
        let mut emitter = Emitter::new();
        assert_eq!(
            emit_all(&mut emitter, events).unwrap_err().problem,
            INVALID_UTF8
        );
    }

    // Not upstream's: into_output gives the bytes output shows.
    #[test]
    fn into_output_matches_output() {
        let mut emitter = Emitter::new();
        emit_all(
            &mut emitter,
            scalar_stream(b"x", b"", ScalarStyle::DoubleQuoted),
        )
        .unwrap();
        let out = emitter.output().to_vec();
        assert!(!out.is_empty());
        assert_eq!(emitter.into_output(), out);
    }

    // Not upstream's: set_encoding a second time is an error where yaml.v3
    // panics.
    #[test]
    fn set_encoding_twice() {
        let mut emitter = Emitter::new();
        emitter.set_encoding(Encoding::Utf8).unwrap();
        let err = emitter.set_encoding(Encoding::Utf8).unwrap_err();
        assert_eq!(err.problem, "must set the output encoding only once");
        assert_eq!(err.kind, ErrorType::Emitter);
    }

    // Not upstream's: after an error every later call returns it again.
    #[test]
    fn errors_are_sticky() {
        let mut emitter = Emitter::new();
        let err = emitter.emit(Event::stream_end()).unwrap_err();
        assert_eq!(err.problem, "expected STREAM-START");
        let again = emitter
            .emit(Event::stream_start(Encoding::Utf8))
            .unwrap_err();
        assert_eq!(again, err);
        assert_eq!(emitter.flush().unwrap_err(), err);
        assert!(emitter.output().is_empty());
    }

    // Not upstream's: settings yaml.v3 would divide by zero or panic on,
    // set after STREAM-START, give errors.
    #[test]
    fn bad_settings_after_stream_start() {
        let mut emitter = Emitter::new();
        emitter.emit(Event::stream_start(Encoding::Utf8)).unwrap();
        emitter.set_best_indent(0);
        let block = CollectionStyle::Block;
        let plain = |v: &[u8]| {
            Event::scalar(
                Vec::new(),
                Vec::new(),
                v.to_vec(),
                true,
                true,
                ScalarStyle::Plain,
            )
        };
        let events = vec![
            Event::document_start(None, Vec::new(), true),
            Event::mapping_start(Vec::new(), Vec::new(), true, block),
            plain(b"k"),
            Event::mapping_start(Vec::new(), Vec::new(), true, block),
            plain(b"k"),
            plain(b"v"),
            Event::mapping_end(),
            Event::mapping_end(),
            Event::document_end(true),
            Event::stream_end(),
        ];
        assert!(emit_all(&mut emitter, events).is_err());

        let mut emitter = Emitter::new();
        emitter.emit(Event::stream_start(Encoding::Utf8)).unwrap();
        emitter.set_break(Break::Any);
        let mut events = scalar_stream(b"x", b"", ScalarStyle::Plain);
        events.remove(0);
        let err = emit_all(&mut emitter, events).unwrap_err();
        assert_eq!(err.problem, "unknown line break setting");
    }

    // Not upstream's: set_indent clamps to 2..=9 at once; set_best_indent
    // leaves the clamp to STREAM-START, as yaml.v3's encoder does.
    #[test]
    fn indent_settings() {
        let mut emitter = Emitter::new();
        emitter.set_indent(12);
        assert_eq!(emitter.best_indent, 2);
        emitter.set_indent(5);
        assert_eq!(emitter.best_indent, 5);
        emitter.set_best_indent(12);
        assert_eq!(emitter.best_indent, 12);
        emitter.emit(Event::stream_start(Encoding::Utf8)).unwrap();
        assert_eq!(emitter.best_indent, 2);
    }

    // Not upstream's: Debug output doesn't show the text being written.
    #[test]
    fn debug_hides_text() {
        let mut emitter = Emitter::new();
        emitter.emit(Event::stream_start(Encoding::Utf8)).unwrap();
        emitter
            .emit(Event::document_start(None, Vec::new(), true))
            .unwrap();
        let ev = Event::scalar(
            Vec::new(),
            Vec::new(),
            b"hidden-value".to_vec(),
            true,
            true,
            ScalarStyle::Plain,
        );
        emitter.emit(ev).unwrap();
        let shown = format!("{emitter:?}");
        assert!(!shown.contains("hidden-value"), "{shown}");
    }
}
