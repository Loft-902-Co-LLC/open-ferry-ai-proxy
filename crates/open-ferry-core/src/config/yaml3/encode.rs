// Ported from gopkg.in/yaml.v3 v3.0.1 encode.go (encoder, newEncoder,
// init, finish, emit, must, marshalDoc, marshal, mapv, structv, mappingv,
// slicev, isBase60Float, isOldBool, stringv, boolv, intv, uintv, floatv,
// nilv, emitScalar, nodev, node), sorter.go (keyList.Less, keyFloat,
// numLess), yaml.go (Marshal, Encoder.Encode, Encoder.SetIndent,
// Encoder.Close, isZero) (Apache-2.0), the YAML library CLIProxyAPI v8.0.20
// (MIT) reads and writes its config with, and Go's time.Duration.String
// (BSD-3-Clause) and strconv.FormatFloat(f, 'g', -1, 64), which it calls.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/go-yaml/yaml
//
// Copyright (c) 2011-2019 Canonical Ltd
// Licensed under the Apache License, Version 2.0; see licenses/go-yaml-LICENSE
// and licenses/go-yaml-NOTICE.

//! Turns a [`Node`] tree, or a Go-shaped [`Value`], into emitter events and
//! bytes, as yaml.v3's encoder does.
//!
//! [`Value`] stands for the Go values upstream marshals: its config
//! structs, maps, slices, strings, numbers and durations. Whoever builds a
//! [`Value::Struct`] lists the fields a Go struct would write, in order,
//! after `omitempty`; [`Value::is_zero`] is yaml.v3's `isZero` for that.
//!
//! Deviations from upstream:
//! - Strings are always valid UTF-8, so the `!!binary` fallback for invalid
//!   UTF-8 and its errors don't exist. Values are never tagged.
//! - A tree nested deeper than [`MAX_DEPTH`] is refused (`yaml: exceeded
//!   max depth of 256`) where yaml.v3 recurses until Go's stack limit.
//! - Map keys that compare equal under yaml.v3's order keep the order they
//!   were given in; Go's order for them depends on map iteration, which is
//!   random.

use super::compose::YamlError;
use super::emitter::Emitter;
use super::types::{CollectionStyle, Encoding, Event, ScalarStyle};
use super::{Kind, MAP_TAG, MAX_DEPTH, Node, SEQ_TAG, STR_TAG, Style, long_tag, short_tag};
use regex::Regex;
use std::cmp::Ordering;
use std::sync::LazyLock;

/// A Go value for the encoder to marshal.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    /// A nil pointer, interface, map or slice behind an interface: `null`.
    Null,
    /// A `bool`.
    Bool(bool),
    /// A signed integer.
    Int(i64),
    /// An unsigned integer.
    Uint(u64),
    /// A `float64`.
    Float(f64),
    /// A `string`.
    Str(String),
    /// A `time.Duration` in nanoseconds, written as its `String()`.
    Duration(i64),
    /// A slice or array.
    Seq(Vec<Value>),
    /// A map, written in yaml.v3's key order whatever order it's given in.
    Map(Vec<(Value, Value)>),
    /// A struct: the fields it writes, in order.
    Struct(Vec<Field>),
    /// A `yaml.Node`, or what a `MarshalYAML` returning one writes.
    Node(Node),
}

/// A struct field to write.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Field {
    /// The key (the field's `yaml` tag name).
    pub(crate) key: String,
    /// The value.
    pub(crate) value: Value,
    /// Whether the field is tagged `,flow`.
    pub(crate) flow: bool,
}

impl Field {
    /// A field without `,flow`.
    pub(crate) fn new(key: &str, value: Value) -> Self {
        Self {
            key: key.to_owned(),
            value,
            flow: false,
        }
    }
}

impl Value {
    /// A string value.
    pub(crate) fn str(value: &str) -> Self {
        Self::Str(value.to_owned())
    }

    /// yaml.go's `isZero`, which `omitempty` tests.
    pub(crate) fn is_zero(&self) -> bool {
        match self {
            Self::Null => true,
            Self::Bool(value) => !value,
            Self::Int(value) | Self::Duration(value) => *value == 0,
            Self::Uint(value) => *value == 0,
            Self::Float(value) => *value == 0.0,
            Self::Str(value) => value.is_empty(),
            Self::Seq(items) => items.is_empty(),
            Self::Map(entries) => entries.is_empty(),
            Self::Struct(fields) => fields.iter().all(|field| field.value.is_zero()),
            Self::Node(node) => node.is_zero(),
        }
    }

    /// The `reflect.Kind` yaml.v3's key order compares.
    fn reflect_kind(&self) -> u8 {
        match self {
            Self::Bool(_) => 1,
            Self::Int(_) => 2,
            Self::Duration(_) => 6,
            Self::Uint(_) => 7,
            Self::Float(_) => 14,
            Self::Null => 20,
            Self::Map(_) => 21,
            Self::Seq(_) => 23,
            Self::Str(_) => 24,
            Self::Struct(_) | Self::Node(_) => 25,
        }
    }
}

/// `yaml.Marshal`: `value` as a YAML document, indented by 4.
pub(crate) fn marshal(value: &Value) -> Result<Vec<u8>, YamlError> {
    let mut encoder = Encoder::new();
    encoder.encode(value)?;
    encoder.close()
}

/// A `yaml.Encoder` writing `node` with `SetIndent(indent)`, then closed:
/// how upstream writes its config file (indent 2).
pub(crate) fn marshal_node(node: &Node, indent: usize) -> Result<Vec<u8>, YamlError> {
    let mut encoder = Encoder::new();
    encoder.set_indent(indent);
    encoder.encode_node(node)?;
    encoder.close()
}

/// yaml.v3's `encoder`, as a `yaml.Encoder` writing to memory.
pub(crate) struct Encoder {
    emitter: Emitter,
    /// The next collection is written in flow style.
    flow: bool,
    indent: usize,
    done_init: bool,
}

impl Encoder {
    /// `newEncoder`.
    pub(crate) fn new() -> Self {
        let mut emitter = Emitter::new();
        emitter.set_unicode(true);
        Self {
            emitter,
            flow: false,
            indent: 0,
            done_init: false,
        }
    }

    /// `Encoder.SetIndent`.
    pub(crate) fn set_indent(&mut self, spaces: usize) {
        self.indent = spaces;
    }

    /// `encoder.init`.
    fn init(&mut self) -> Result<(), YamlError> {
        if self.done_init {
            return Ok(());
        }
        if self.indent == 0 {
            self.indent = 4;
        }
        self.emitter
            .set_best_indent(i32::try_from(self.indent).unwrap_or(i32::MAX));
        self.emit(Event::stream_start(Encoding::Utf8))?;
        self.done_init = true;
        Ok(())
    }

    /// `encoder.finish`.
    fn finish(&mut self) -> Result<(), YamlError> {
        self.emitter.set_open_ended(false);
        self.emit(Event::stream_end())
    }

    /// `encoder.emit` and `encoder.must`.
    fn emit(&mut self, event: Event) -> Result<(), YamlError> {
        self.emitter
            .emit(event)
            .map_err(|error| YamlError::fail(error.message()))
    }

    /// `Encoder.Encode(node)` for a `*yaml.Node` (`marshalDoc`).
    pub(crate) fn encode_node(&mut self, node: &Node) -> Result<(), YamlError> {
        self.init()?;
        if node.kind == Kind::Document {
            return self.node(node, "", false, 0);
        }
        self.emit(Event::document_start(None, Vec::new(), true))?;
        self.node(node, "", false, 0)?;
        self.emit(Event::document_end(true))
    }

    /// `Encoder.Encode(value)` (`marshalDoc`).
    pub(crate) fn encode(&mut self, value: &Value) -> Result<(), YamlError> {
        if let Value::Node(node) = value {
            return self.encode_node(node);
        }
        self.init()?;
        self.emit(Event::document_start(None, Vec::new(), true))?;
        self.marshal(value, 0)?;
        self.emit(Event::document_end(true))
    }

    /// `Encoder.Close`: ends the stream and gives the bytes written.
    pub(crate) fn close(mut self) -> Result<Vec<u8>, YamlError> {
        self.finish()?;
        Ok(self.emitter.into_output())
    }

    /// `encoder.marshal`, for untagged values.
    fn marshal(&mut self, value: &Value, depth: usize) -> Result<(), YamlError> {
        if depth > MAX_DEPTH {
            return Err(depth_error());
        }
        let depth = depth.saturating_add(1);
        match value {
            Value::Null => self.nilv(),
            Value::Node(node) => self.node(node, "", false, depth),
            Value::Duration(nanos) => self.stringv(&duration_string(*nanos)),
            Value::Str(text) => self.stringv(text),
            Value::Bool(value) => self.plain(if *value { "true" } else { "false" }),
            Value::Int(value) => self.plain(&value.to_string()),
            Value::Uint(value) => self.plain(&value.to_string()),
            Value::Float(value) => self.plain(&float_string(*value)),
            Value::Seq(items) => {
                self.sequence_start()?;
                for item in items {
                    self.marshal(item, depth)?;
                }
                self.emit(Event::sequence_end())
            }
            Value::Map(entries) => {
                self.mapping_start()?;
                for (key, value) in sorted_entries(entries) {
                    self.marshal(key, depth)?;
                    self.marshal(value, depth)?;
                }
                self.emit(Event::mapping_end())
            }
            Value::Struct(fields) => {
                self.mapping_start()?;
                for field in fields {
                    self.stringv(&field.key)?;
                    self.flow = field.flow;
                    self.marshal(&field.value, depth)?;
                }
                self.emit(Event::mapping_end())
            }
        }
    }

    /// The start of `encoder.mappingv`.
    fn mapping_start(&mut self) -> Result<(), YamlError> {
        let style = if std::mem::take(&mut self.flow) {
            CollectionStyle::Flow
        } else {
            CollectionStyle::Block
        };
        self.emit(Event::mapping_start(Vec::new(), Vec::new(), true, style))
    }

    /// The start of `encoder.slicev`.
    fn sequence_start(&mut self) -> Result<(), YamlError> {
        let style = if std::mem::take(&mut self.flow) {
            CollectionStyle::Flow
        } else {
            CollectionStyle::Block
        };
        self.emit(Event::sequence_start(Vec::new(), Vec::new(), true, style))
    }

    /// `encoder.stringv`, untagged.
    fn stringv(&mut self, text: &str) -> Result<(), YamlError> {
        // Check to see if it would resolve to a specific tag when encoded
        // unquoted. If it doesn't, there's no need to quote it.
        let can_use_plain =
            super::resolve_tag(text) == STR_TAG && !(is_base60_float(text) || is_old_bool(text));
        let style = if text.contains('\n') {
            if self.flow {
                ScalarStyle::DoubleQuoted
            } else {
                ScalarStyle::Literal
            }
        } else if can_use_plain {
            ScalarStyle::Plain
        } else {
            ScalarStyle::DoubleQuoted
        };
        self.emit_scalar(text, "", "", style, Comments::default())
    }

    /// `boolv`, `intv`, `uintv` and `floatv`: a plain, untagged scalar.
    fn plain(&mut self, text: &str) -> Result<(), YamlError> {
        self.emit_scalar(text, "", "", ScalarStyle::Plain, Comments::default())
    }

    /// `encoder.nilv`.
    fn nilv(&mut self) -> Result<(), YamlError> {
        self.plain("null")
    }

    /// `encoder.emitScalar`.
    fn emit_scalar(
        &mut self,
        value: &str,
        anchor: &str,
        tag: &str,
        style: ScalarStyle,
        comments: Comments<'_>,
    ) -> Result<(), YamlError> {
        let implicit = tag.is_empty();
        let tag = if implicit {
            String::new()
        } else {
            long_tag(tag)
        };
        let mut event = Event::scalar(
            anchor.as_bytes().to_vec(),
            tag.into_bytes(),
            value.as_bytes().to_vec(),
            implicit,
            implicit,
            style,
        );
        event.head_comment = comments.head.as_bytes().to_vec();
        event.line_comment = comments.line.as_bytes().to_vec();
        event.foot_comment = comments.foot.as_bytes().to_vec();
        event.tail_comment = comments.tail.as_bytes().to_vec();
        self.emit(event)
    }

    /// `encoder.node`. With `drop_foot`, the node is written without its
    /// foot comment, as yaml.v3 writes a copy of a mapping key whose foot
    /// it moves to the next key's tail.
    fn node(
        &mut self,
        node: &Node,
        tail: &str,
        drop_foot: bool,
        depth: usize,
    ) -> Result<(), YamlError> {
        if depth > MAX_DEPTH {
            return Err(depth_error());
        }
        let depth = depth.saturating_add(1);
        let foot = if drop_foot {
            ""
        } else {
            node.foot_comment.as_str()
        };

        // Zero nodes behave as nil.
        if node.kind == Kind::Zero && node.is_zero() {
            return self.nilv();
        }

        // If the tag was not explicitly requested, and dropping it won't
        // change the implicit tag of the value, don't include it in the
        // presentation.
        let mut tag = node.tag.as_str();
        let stag = short_tag(tag);
        let mut force_quoting = false;
        if !tag.is_empty() && !node.style.has(Style::TAGGED) {
            if node.kind == Kind::Scalar {
                if stag == STR_TAG
                    && node.style.has(
                        Style::SINGLE_QUOTED
                            | Style::DOUBLE_QUOTED
                            | Style::LITERAL
                            | Style::FOLDED,
                    )
                {
                    tag = "";
                } else {
                    let rtag = super::resolve_tag(&node.value);
                    if rtag == stag {
                        tag = "";
                    } else if stag == STR_TAG {
                        tag = "";
                        force_quoting = true;
                    }
                }
            } else {
                let rtag = match node.kind {
                    Kind::Mapping => MAP_TAG,
                    Kind::Sequence => SEQ_TAG,
                    _ => "",
                };
                if rtag == stag {
                    tag = "";
                }
            }
        }

        match node.kind {
            Kind::Document => {
                let mut event = Event::document_start(None, Vec::new(), true);
                event.head_comment = node.head_comment.as_bytes().to_vec();
                self.emit(event)?;
                for child in &node.content {
                    self.node(child, "", false, depth)?;
                }
                let mut event = Event::document_end(true);
                event.foot_comment = foot.as_bytes().to_vec();
                self.emit(event)
            }
            Kind::Sequence => {
                let style = if node.style.has(Style::FLOW) {
                    CollectionStyle::Flow
                } else {
                    CollectionStyle::Block
                };
                let mut event = Event::sequence_start(
                    node.anchor.as_bytes().to_vec(),
                    long_tag(tag).into_bytes(),
                    tag.is_empty(),
                    style,
                );
                event.head_comment = node.head_comment.as_bytes().to_vec();
                self.emit(event)?;
                for child in &node.content {
                    self.node(child, "", false, depth)?;
                }
                let mut event = Event::sequence_end();
                event.line_comment = node.line_comment.as_bytes().to_vec();
                event.foot_comment = foot.as_bytes().to_vec();
                self.emit(event)
            }
            Kind::Mapping => {
                let style = if node.style.has(Style::FLOW) {
                    CollectionStyle::Flow
                } else {
                    CollectionStyle::Block
                };
                let mut event = Event::mapping_start(
                    node.anchor.as_bytes().to_vec(),
                    long_tag(tag).into_bytes(),
                    tag.is_empty(),
                    style,
                );
                event.tail_comment = tail.as_bytes().to_vec();
                event.head_comment = node.head_comment.as_bytes().to_vec();
                self.emit(event)?;

                // The tail logic below moves the foot comment of prior keys
                // to the following key, since the value for each key may be
                // a nested structure and the foot needs to be processed only
                // the entirety of the value is streamed. The last tail is
                // processed with the mapping end event.
                let mut tail = "";
                for [key, value] in node.content.as_chunks::<2>().0 {
                    let key_foot = key.foot_comment.as_str();
                    self.node(key, tail, !key_foot.is_empty(), depth)?;
                    tail = key_foot;
                    self.node(value, "", false, depth)?;
                }

                let mut event = Event::mapping_end();
                event.tail_comment = tail.as_bytes().to_vec();
                event.line_comment = node.line_comment.as_bytes().to_vec();
                event.foot_comment = foot.as_bytes().to_vec();
                self.emit(event)
            }
            Kind::Alias => {
                let mut event = Event::alias(node.value.as_bytes().to_vec());
                event.head_comment = node.head_comment.as_bytes().to_vec();
                event.line_comment = node.line_comment.as_bytes().to_vec();
                event.foot_comment = foot.as_bytes().to_vec();
                self.emit(event)
            }
            Kind::Scalar => {
                let style = if node.style.has(Style::DOUBLE_QUOTED) {
                    ScalarStyle::DoubleQuoted
                } else if node.style.has(Style::SINGLE_QUOTED) {
                    ScalarStyle::SingleQuoted
                } else if node.style.has(Style::LITERAL) {
                    ScalarStyle::Literal
                } else if node.style.has(Style::FOLDED) {
                    ScalarStyle::Folded
                } else if node.value.contains('\n') {
                    ScalarStyle::Literal
                } else if force_quoting {
                    ScalarStyle::DoubleQuoted
                } else {
                    ScalarStyle::Plain
                };
                self.emit_scalar(
                    &node.value,
                    &node.anchor,
                    tag,
                    style,
                    Comments {
                        head: &node.head_comment,
                        line: &node.line_comment,
                        foot,
                        tail,
                    },
                )
            }
            Kind::Zero => Err(YamlError::fail("cannot encode node with unknown kind 0")),
        }
    }
}

/// The comments `emitScalar` puts on a scalar event.
#[derive(Clone, Copy, Default)]
struct Comments<'a> {
    head: &'a str,
    line: &'a str,
    foot: &'a str,
    tail: &'a str,
}

/// The error for a tree nested too deeply to write.
fn depth_error() -> YamlError {
    YamlError::fail(format!("exceeded max depth of {MAX_DEPTH}"))
}

/// `isBase60Float`: whether `s` is in YAML 1.1's base 60 notation, which
/// is written quoted for other parsers' sake.
fn is_base60_float(s: &str) -> bool {
    /// From http://yaml.org/type/float.html, except the regular expression
    /// there is bogus. In practice parsers do not enforce the "\.[0-9_]*"
    /// suffix.
    static BASE60_FLOAT: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"^[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+(?:\.[0-9_]*)?$").ok());
    // Fast path.
    let Some(&first) = s.as_bytes().first() else {
        return false;
    };
    if !(first == b'+' || first == b'-' || first.is_ascii_digit()) || !s.contains(':') {
        return false;
    }
    // Do the full match.
    BASE60_FLOAT
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(s))
}

/// `isOldBool`: whether `s` is a YAML 1.1 boolean, which is written quoted
/// so the output stays valid YAML 1.1.
fn is_old_bool(s: &str) -> bool {
    matches!(
        s,
        "y" | "Y"
            | "yes"
            | "Yes"
            | "YES"
            | "on"
            | "On"
            | "ON"
            | "n"
            | "N"
            | "no"
            | "No"
            | "NO"
            | "off"
            | "Off"
            | "OFF"
    )
}

/// `strconv.FormatFloat(value, 'g', -1, 64)` as `floatv` writes it, with
/// `.inf`, `-.inf` and `.nan`.
fn float_string(value: f64) -> String {
    if value.is_nan() {
        return ".nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { ".inf" } else { "-.inf" }.to_owned();
    }
    // Rust's `{:e}` gives the shortest digits that read back as `value`,
    // as Go's shortest formatting does.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let (negative, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, mantissa),
    };
    let digits: Vec<u8> = mantissa.bytes().filter(u8::is_ascii_digit).collect();
    let exp: i64 = exponent.parse().unwrap_or(0);
    // The decimal point's position, as Go's `decimalSlice.dp`.
    let dp = exp.saturating_add(1);
    let nd = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    let digit = |at: i64| -> char {
        usize::try_from(at)
            .ok()
            .and_then(|at| digits.get(at))
            .map_or('0', |&d| char::from(d))
    };
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    // %e is used if the exponent from the conversion is less than -4 or
    // greater than or equal to the precision (6 for the shortest).
    if !(-4..6).contains(&exp) {
        // fmtE with nd-1 digits after the point.
        out.push(digit(0));
        if nd > 1 {
            out.push('.');
            for at in 1..nd {
                out.push(digit(at));
            }
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        let magnitude = exp.unsigned_abs();
        if magnitude < 10 {
            out.push('0');
        }
        out.push_str(&magnitude.to_string());
    } else {
        // fmtF with max(nd-dp, 0) digits after the point.
        if dp > 0 {
            for at in 0..dp {
                out.push(digit(at));
            }
        } else {
            out.push('0');
        }
        let prec = nd.saturating_sub(dp).max(0);
        if prec > 0 {
            out.push('.');
            for i in 0..prec {
                out.push(digit(dp.saturating_add(i)));
            }
        }
    }
    out
}

/// `time.Duration.String`: `1h2m3.5s`, `1.5ms`, `0s`.
pub(crate) fn duration_string(nanos: i64) -> String {
    const SECOND: u64 = 1_000_000_000;
    let negative = nanos < 0;
    let mut u = nanos.unsigned_abs();
    // Built backwards, as Go fills its buffer from the end.
    let mut out: Vec<char> = Vec::new();
    if u < SECOND {
        // Smaller than a second: smaller units, like 1.2ms.
        out.push('s');
        let prec;
        if u == 0 {
            return "0s".to_owned();
        } else if u < 1_000 {
            prec = 0;
            out.push('n');
        } else if u < 1_000_000 {
            prec = 3;
            out.push('\u{b5}');
        } else {
            prec = 6;
            out.push('m');
        }
        u = push_frac(&mut out, u, prec);
        push_int(&mut out, u);
    } else {
        out.push('s');
        u = push_frac(&mut out, u, 9);
        // u is now integer seconds.
        push_int(&mut out, u % 60);
        u /= 60;
        // u is now integer minutes.
        if u > 0 {
            out.push('m');
            push_int(&mut out, u % 60);
            u /= 60;
            // u is now integer hours.
            if u > 0 {
                out.push('h');
                push_int(&mut out, u);
            }
        }
    }
    if negative {
        out.push('-');
    }
    out.iter().rev().collect()
}

/// `fmtFrac`, backwards: the fraction of `v/10**prec` without trailing
/// zeros, and the point only if there is a fraction. Gives the integer
/// part.
fn push_frac(out: &mut Vec<char>, mut v: u64, prec: u32) -> u64 {
    let mut print = false;
    for _ in 0..prec {
        let digit = v % 10;
        print = print || digit != 0;
        if print {
            out.push(digit_char(digit));
        }
        v /= 10;
    }
    if print {
        out.push('.');
    }
    v
}

/// `fmtInt`, backwards.
fn push_int(out: &mut Vec<char>, mut v: u64) {
    if v == 0 {
        out.push('0');
        return;
    }
    while v > 0 {
        out.push(digit_char(v % 10));
        v /= 10;
    }
}

/// The decimal digit `digit` (0-9).
fn digit_char(digit: u64) -> char {
    u32::try_from(digit)
        .ok()
        .and_then(|digit| char::from_digit(digit, 10))
        .unwrap_or('0')
}

/// A map's entries in yaml.v3's key order (`sort.Sort(keyList)`).
fn sorted_entries(entries: &[(Value, Value)]) -> Vec<&(Value, Value)> {
    let mut sorted: Vec<&(Value, Value)> = entries.iter().collect();
    merge_sort(&mut sorted, &|a, b| key_less(&a.0, &b.0));
    sorted
}

/// A stable merge sort by `less`, which needn't be a total order (Rust's
/// own sorts may panic on one that isn't).
fn merge_sort<T: Copy>(items: &mut [T], less: &dyn Fn(&T, &T) -> bool) {
    if items.len() < 2 {
        return;
    }
    let len = items.len();
    let (left, right) = items.split_at_mut(len / 2);
    merge_sort(left, less);
    merge_sort(right, less);
    let mut merged = Vec::with_capacity(len);
    let (mut left, mut right) = (left.iter().peekable(), right.iter().peekable());
    loop {
        match (left.peek(), right.peek()) {
            (Some(a), Some(b)) => {
                if less(b, a) {
                    merged.extend(right.next().copied());
                } else {
                    merged.extend(left.next().copied());
                }
            }
            (Some(_), None) => merged.extend(left.next().copied()),
            (None, Some(_)) => merged.extend(right.next().copied()),
            (None, None) => break,
        }
    }
    for (slot, item) in items.iter_mut().zip(merged) {
        *slot = item;
    }
}

/// `keyList.Less`.
fn key_less(a: &Value, b: &Value) -> bool {
    let (ak, bk) = (a.reflect_kind(), b.reflect_kind());
    if let (Some(af), Some(bf)) = (key_float(a), key_float(b)) {
        if af != bf {
            return af < bf;
        }
        if ak != bk {
            return ak < bk;
        }
        return num_less(a, b);
    }
    let (Value::Str(a), Value::Str(b)) = (a, b) else {
        return ak < bk;
    };
    let ar: Vec<char> = a.chars().collect();
    let br: Vec<char> = b.chars().collect();
    let mut digits = false;
    for (i, (&ac, &bc)) in ar.iter().zip(&br).enumerate() {
        if ac == bc {
            digits = is_digit(ac);
            continue;
        }
        let al = is_letter(ac);
        let bl = is_letter(bc);
        if al && bl {
            return ac < bc;
        }
        if al || bl {
            return if digits { al } else { bl };
        }
        let (mut an, mut bn): (i64, i64) = (0, 0);
        if ac == '0' || bc == '0' {
            for &c in ar.get(..i).unwrap_or_default().iter().rev() {
                if !is_digit(c) {
                    break;
                }
                if c != '0' {
                    an = 1;
                    bn = 1;
                    break;
                }
            }
        }
        let mut ai = i;
        while let Some(&c) = ar.get(ai).filter(|&&c| is_digit(c)) {
            an = an.wrapping_mul(10).wrapping_add(digit_value(c));
            ai += 1;
        }
        let mut bi = i;
        while let Some(&c) = br.get(bi).filter(|&&c| is_digit(c)) {
            bn = bn.wrapping_mul(10).wrapping_add(digit_value(c));
            bi += 1;
        }
        if an != bn {
            return an < bn;
        }
        if ai != bi {
            return ai < bi;
        }
        return ac < bc;
    }
    ar.len() < br.len()
}

/// `int64(r - '0')` for a digit rune, as Go computes it.
fn digit_value(c: char) -> i64 {
    i64::from(u32::from(c)) - i64::from(u32::from('0'))
}

/// `keyFloat`: a number or bool key as a float.
fn key_float(value: &Value) -> Option<f64> {
    match value {
        Value::Int(v) | Value::Duration(v) => Some(*v as f64),
        Value::Uint(v) => Some(*v as f64),
        Value::Float(v) => Some(*v),
        Value::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// `numLess`, for keys of the same kind.
fn num_less(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) | (Value::Duration(a), Value::Duration(b)) => a < b,
        (Value::Uint(a), Value::Uint(b)) => a < b,
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b) == Some(Ordering::Less),
        (Value::Bool(a), Value::Bool(b)) => !a && *b,
        _ => false,
    }
}

/// Go's `unicode.IsDigit`: a decimal digit (`Nd`).
fn is_digit(c: char) -> bool {
    static DIGIT: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^\p{Nd}$").ok());
    if c.is_ascii() {
        return c.is_ascii_digit();
    }
    DIGIT
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(c.encode_utf8(&mut [0; 4])))
}

/// Go's `unicode.IsLetter`: a letter (`L`).
fn is_letter(c: char) -> bool {
    static LETTER: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^\p{L}$").ok());
    if c.is_ascii() {
        return c.is_ascii_alphabetic();
    }
    LETTER
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(c.encode_utf8(&mut [0; 4])))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: Go's strconv.FormatFloat(f, 'g', -1, 64) answers,
    // recorded with Go.
    #[test]
    fn floats_format_as_go() {
        for (value, want) in [
            (0.0, "0"),
            (-0.0, "-0"),
            (1.0, "1"),
            (1.5, "1.5"),
            (100000.0, "100000"),
            (1000000.0, "1e+06"),
            (123456789.0, "1.23456789e+08"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.25e-7, "1.25e-07"),
            (-2.5, "-2.5"),
            (1e100, "1e+100"),
            (0.1, "0.1"),
            (f64::INFINITY, ".inf"),
            (f64::NEG_INFINITY, "-.inf"),
            (f64::NAN, ".nan"),
        ] {
            assert_eq!(float_string(value), want, "{value}");
        }
    }

    // Not upstream's: Go's time.Duration.String answers, recorded with Go.
    #[test]
    fn durations_format_as_go() {
        for (nanos, want) in [
            (0, "0s"),
            (1, "1ns"),
            (1_500, "1.5\u{b5}s"),
            (1_500_000, "1.5ms"),
            (1_000_000_000, "1s"),
            (90_000_000_000, "1m30s"),
            (3_600_000_000_000, "1h0m0s"),
            (3_723_500_000_000, "1h2m3.5s"),
            (-2_000_000_000, "-2s"),
        ] {
            assert_eq!(duration_string(nanos), want, "{nanos}");
        }
    }
}
