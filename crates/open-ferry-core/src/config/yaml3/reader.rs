// Ported from gopkg.in/yaml.v3 v3.0.1 readerc.go (yaml_parser_set_reader_error,
// yaml_parser_determine_encoding, yaml_parser_update_raw_buffer,
// yaml_parser_update_buffer) and apic.go (yaml_string_read_handler) (MIT,
// from libyaml), the YAML library CLIProxyAPI v8.0.20 (MIT) reads and writes
// its config with.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/go-yaml/yaml
//
// Copyright (c) 2006-2010 Kirill Simonov
// Copyright (c) 2006-2011 Kirill Simonov
// Copyright (c) 2011-2019 Canonical Ltd
// Licensed under the MIT License; see licenses/go-yaml-LICENSE.

//! The reader: it detects the input's encoding from its byte order mark,
//! decodes UTF-8 or UTF-16 into the UTF-8 working buffer the scanner reads,
//! and rejects invalid sequences and control characters.
//!
//! As in yaml.v3 the input is read lazily: at most
//! [`INPUT_RAW_BUFFER_SIZE`] raw bytes at a time, decoded only when the
//! scanner asks for more characters than the buffer holds. An invalid byte
//! late in the input is therefore reported only when the scanner gets near
//! it, after the events before it have been produced, with the byte offset
//! yaml.v3 reports.
//!
//! Deviations from upstream:
//! - Indexing is panic-free: bytes past the end of a buffer read as NUL.
//! - After a decoding error yaml.v3 leaves its working buffer open to its
//!   full capacity, so stale bytes from earlier reads sit past the decoded
//!   ones; here the buffer ends at the decoded bytes and past them reads as
//!   NUL. The scanner never reads past the characters it has counted, so
//!   this can't change a result.
//! - The working buffer grows as needed where yaml.v3's has a fixed
//!   capacity of [`INPUT_BUFFER_SIZE`](super::chars::INPUT_BUFFER_SIZE)
//!   bytes; yaml.v3 never fills it.
//! - Only string input is supported (`yaml_parser_set_input_string`), so
//!   the `input error` of a failing `io.Reader` can't occur.

use super::chars::{INPUT_RAW_BUFFER_SIZE, at};
use super::parser::Parser;
use super::types::{Encoding, ErrorType};

/// The UTF-8 byte order mark (`bom_UTF8`).
const BOM_UTF8: [u8; 3] = [0xEF, 0xBB, 0xBF];
/// The UTF-16LE byte order mark (`bom_UTF16LE`).
const BOM_UTF16LE: [u8; 2] = [0xFF, 0xFE];
/// The UTF-16BE byte order mark (`bom_UTF16BE`).
const BOM_UTF16BE: [u8; 2] = [0xFE, 0xFF];

impl Parser<'_> {
    /// yaml_parser_set_reader_error: set the reader error and return false.
    pub(super) fn set_reader_error(
        &mut self,
        problem: &'static str,
        offset: usize,
        value: i64,
    ) -> bool {
        self.error = ErrorType::Reader;
        self.problem = problem;
        self.problem_offset = offset;
        self.problem_value = value;
        false
    }

    /// yaml_parser_determine_encoding: determine the input stream encoding
    /// by checking the BOM symbol. If no BOM is found, the UTF-8 encoding is
    /// assumed.
    fn determine_encoding(&mut self) -> bool {
        // Ensure that we had enough bytes in the raw buffer.
        while !self.eof && self.raw_buffer.len().saturating_sub(self.raw_buffer_pos) < 3 {
            if !self.update_raw_buffer() {
                return false;
            }
        }

        // Determine the encoding.
        let buf = &self.raw_buffer;
        let pos = self.raw_buffer_pos;
        let avail = buf.len().saturating_sub(pos);
        let b0 = at(buf, pos);
        let b1 = at(buf, pos + 1);
        let b2 = at(buf, pos + 2);
        if avail >= 2 && b0 == BOM_UTF16LE[0] && b1 == BOM_UTF16LE[1] {
            self.encoding = Encoding::Utf16Le;
            self.raw_buffer_pos += 2;
            self.offset += 2;
        } else if avail >= 2 && b0 == BOM_UTF16BE[0] && b1 == BOM_UTF16BE[1] {
            self.encoding = Encoding::Utf16Be;
            self.raw_buffer_pos += 2;
            self.offset += 2;
        } else if avail >= 3 && b0 == BOM_UTF8[0] && b1 == BOM_UTF8[1] && b2 == BOM_UTF8[2] {
            self.encoding = Encoding::Utf8;
            self.raw_buffer_pos += 3;
            self.offset += 3;
        } else {
            self.encoding = Encoding::Utf8;
        }
        true
    }

    /// yaml_parser_update_raw_buffer: move the unread raw bytes to the
    /// beginning of the raw buffer and fill the rest from the input.
    fn update_raw_buffer(&mut self) -> bool {
        // Return if the raw buffer is full.
        if self.raw_buffer_pos == 0 && self.raw_buffer.len() == INPUT_RAW_BUFFER_SIZE {
            return true;
        }

        // Return on EOF.
        if self.eof {
            return true;
        }

        // Move the remaining bytes in the raw buffer to the beginning.
        let consumed = self.raw_buffer_pos.min(self.raw_buffer.len());
        self.raw_buffer.drain(..consumed);
        self.raw_buffer_pos = 0;

        // Call the read handler to fill the buffer.
        let room = INPUT_RAW_BUFFER_SIZE.saturating_sub(self.raw_buffer.len());
        let size_read = self.string_read_handler(room);
        if size_read == 0 {
            self.eof = true;
        }
        true
    }

    /// yaml_string_read_handler: append up to `room` input bytes to the raw
    /// buffer and return how many; 0 is EOF.
    fn string_read_handler(&mut self, room: usize) -> usize {
        let rest = self.input.get(self.input_pos..).unwrap_or_default();
        if rest.is_empty() {
            return 0;
        }
        let n = room.min(rest.len());
        let chunk = rest.get(..n).unwrap_or_default();
        self.raw_buffer.extend_from_slice(chunk);
        self.input_pos += chunk.len();
        chunk.len()
    }

    /// yaml_parser_update_buffer: ensure that the buffer contains at least
    /// `length` characters. Return true on success, false on failure.
    ///
    /// The length is supposed to be significantly less that the buffer size.
    pub(super) fn update_buffer(&mut self, length: usize) -> bool {
        // [Go] This function was changed to guarantee the requested length
        // size at EOF.

        // Return if the buffer contains enough characters.
        if self.unread >= length {
            return true;
        }

        // Determine the input encoding if it is not known yet.
        if self.encoding == Encoding::Any && !self.determine_encoding() {
            return false;
        }

        // Move the unread characters to the beginning of the buffer.
        let mut buffer_len = self.buffer.len();
        if self.buffer_pos > 0 && self.buffer_pos < buffer_len {
            self.buffer.drain(..self.buffer_pos);
            buffer_len -= self.buffer_pos;
            self.buffer_pos = 0;
        } else if self.buffer_pos == buffer_len {
            buffer_len = 0;
            self.buffer_pos = 0;
        }
        self.buffer.truncate(buffer_len);

        // Fill the buffer until it has enough characters.
        let mut first = true;
        while self.unread < length {
            // Fill the raw buffer if necessary.
            if (!first || self.raw_buffer_pos == self.raw_buffer.len()) && !self.update_raw_buffer()
            {
                return false;
            }
            first = false;

            // Decode the raw buffer.
            while self.raw_buffer_pos != self.raw_buffer.len() {
                let raw_unread = self.raw_buffer.len().saturating_sub(self.raw_buffer_pos);
                let decoded = match self.encoding {
                    Encoding::Utf16Le | Encoding::Utf16Be => self.decode_utf16(raw_unread),
                    _ => self.decode_utf8(raw_unread),
                };
                let (value, width) = match decoded {
                    Decoded::Char(value, width) => (value, width),
                    Decoded::Incomplete => break,
                    Decoded::Error => return false,
                };

                // Check if the character is in the allowed range:
                //      #x9 | #xA | #xD | [#x20-#x7E]               (8 bit)
                //      | #x85 | [#xA0-#xD7FF] | [#xE000-#xFFFD]    (16 bit)
                //      | [#x10000-#x10FFFF]                        (32 bit)
                let allowed = value == 0x09
                    || value == 0x0A
                    || value == 0x0D
                    || (0x20..=0x7E).contains(&value)
                    || value == 0x85
                    || (0xA0..=0xD7FF).contains(&value)
                    || (0xE000..=0xFFFD).contains(&value)
                    || (0x10000..=0x10FFFF).contains(&value);
                if !allowed {
                    return self.set_reader_error(
                        "control characters are not allowed",
                        self.offset,
                        i64::from(value),
                    );
                }

                // Move the raw pointers.
                self.raw_buffer_pos += width;
                self.offset += width;

                // Finally put the character into the buffer.
                push_utf8(&mut self.buffer, value);
                self.unread += 1;
            }

            // On EOF, put NUL into the buffer and return.
            if self.eof {
                self.buffer.push(0);
                self.unread += 1;
                break;
            }
        }
        // [Go] To return true, we need to have the given length in the
        // buffer. This happens here due to the EOF above breaking early.
        if self.buffer.len() < length {
            self.buffer.resize(length, 0);
        }
        true
    }

    /// The UTF-8 part of yaml_parser_update_buffer's decoding loop: decode
    /// the character at the raw buffer position. Check RFC 3629
    /// (<http://www.ietf.org/rfc/rfc3629.txt>) for more details.
    fn decode_utf8(&mut self, raw_unread: usize) -> Decoded {
        let raw = &self.raw_buffer;
        let pos = self.raw_buffer_pos;

        // Determine the length of the UTF-8 sequence.
        let mut octet = at(raw, pos);
        let width = if octet & 0x80 == 0x00 {
            1
        } else if octet & 0xE0 == 0xC0 {
            2
        } else if octet & 0xF0 == 0xE0 {
            3
        } else if octet & 0xF8 == 0xF0 {
            4
        } else {
            // The leading octet is invalid.
            self.set_reader_error("invalid leading UTF-8 octet", self.offset, i64::from(octet));
            return Decoded::Error;
        };

        // Check if the raw buffer contains an incomplete character.
        if width > raw_unread {
            if self.eof {
                self.set_reader_error("incomplete UTF-8 octet sequence", self.offset, -1);
                return Decoded::Error;
            }
            return Decoded::Incomplete;
        }

        // Decode the leading octet.
        let mut value: u32 = match width {
            1 => u32::from(octet & 0x7F),
            2 => u32::from(octet & 0x1F),
            3 => u32::from(octet & 0x0F),
            _ => u32::from(octet & 0x07),
        };

        // Check and decode the trailing octets.
        for k in 1..width {
            octet = at(raw, pos + k);

            // Check if the octet is valid.
            if (octet & 0xC0) != 0x80 {
                self.set_reader_error(
                    "invalid trailing UTF-8 octet",
                    self.offset + k,
                    i64::from(octet),
                );
                return Decoded::Error;
            }

            // Decode the octet.
            value = (value << 6) + u32::from(octet & 0x3F);
        }

        // Check the length of the sequence against the value.
        let length_ok = match width {
            1 => true,
            2 => value >= 0x80,
            3 => value >= 0x800,
            _ => value >= 0x10000,
        };
        if !length_ok {
            self.set_reader_error("invalid length of a UTF-8 sequence", self.offset, -1);
            return Decoded::Error;
        }

        // Check the range of the value.
        if (0xD800..=0xDFFF).contains(&value) || value > 0x10FFFF {
            self.set_reader_error("invalid Unicode character", self.offset, i64::from(value));
            return Decoded::Error;
        }
        Decoded::Char(value, width)
    }

    /// The UTF-16 part of yaml_parser_update_buffer's decoding loop: decode
    /// the character at the raw buffer position, joining surrogate pairs
    /// (RFC 2781, <http://www.ietf.org/rfc/rfc2781.txt>).
    fn decode_utf16(&mut self, raw_unread: usize) -> Decoded {
        let (low, high) = if self.encoding == Encoding::Utf16Le {
            (0, 1)
        } else {
            (1, 0)
        };
        let raw = &self.raw_buffer;
        let pos = self.raw_buffer_pos;

        // Check for incomplete UTF-16 character.
        if raw_unread < 2 {
            if self.eof {
                self.set_reader_error("incomplete UTF-16 character", self.offset, -1);
                return Decoded::Error;
            }
            return Decoded::Incomplete;
        }

        // Get the character.
        let mut value = u32::from(at(raw, pos + low)) + (u32::from(at(raw, pos + high)) << 8);

        // Check for unexpected low surrogate area.
        if value & 0xFC00 == 0xDC00 {
            self.set_reader_error(
                "unexpected low surrogate area",
                self.offset,
                i64::from(value),
            );
            return Decoded::Error;
        }

        // Check for a high surrogate area.
        let width = if value & 0xFC00 == 0xD800 {
            // Check for incomplete surrogate pair.
            if raw_unread < 4 {
                if self.eof {
                    self.set_reader_error("incomplete UTF-16 surrogate pair", self.offset, -1);
                    return Decoded::Error;
                }
                return Decoded::Incomplete;
            }

            // Get the next character.
            let value2 =
                u32::from(at(raw, pos + low + 2)) + (u32::from(at(raw, pos + high + 2)) << 8);

            // Check for a low surrogate area.
            if value2 & 0xFC00 != 0xDC00 {
                self.set_reader_error(
                    "expected low surrogate area",
                    self.offset + 2,
                    i64::from(value2),
                );
                return Decoded::Error;
            }

            // Generate the value of the surrogate pair.
            value = 0x10000 + ((value & 0x3FF) << 10) + (value2 & 0x3FF);
            4
        } else {
            2
        };
        Decoded::Char(value, width)
    }
}

/// The result of decoding one character from the raw buffer.
enum Decoded {
    /// A character and the number of raw bytes it took.
    Char(u32, usize),
    /// The raw buffer ends inside a character, and more input may follow.
    Incomplete,
    /// The reader error is set.
    Error,
}

/// Append `value` to `buf` as UTF-8, as yaml_parser_update_buffer puts a
/// decoded character into the buffer.
fn push_utf8(buf: &mut Vec<u8>, value: u32) {
    // The casts keep the low byte, as Go's byte() conversions do.
    if value <= 0x7F {
        // 0000 0000-0000 007F . 0xxxxxxx
        buf.push(value as u8);
    } else if value <= 0x7FF {
        // 0000 0080-0000 07FF . 110xxxxx 10xxxxxx
        buf.push((0xC0 + (value >> 6)) as u8);
        buf.push((0x80 + (value & 0x3F)) as u8);
    } else if value <= 0xFFFF {
        // 0000 0800-0000 FFFF . 1110xxxx 10xxxxxx 10xxxxxx
        buf.push((0xE0 + (value >> 12)) as u8);
        buf.push((0x80 + ((value >> 6) & 0x3F)) as u8);
        buf.push((0x80 + (value & 0x3F)) as u8);
    } else {
        // 0001 0000-0010 FFFF . 11110xxx 10xxxxxx 10xxxxxx 10xxxxxx
        buf.push((0xF0 + (value >> 18)) as u8);
        buf.push((0x80 + ((value >> 12) & 0x3F)) as u8);
        buf.push((0x80 + ((value >> 6) & 0x3F)) as u8);
        buf.push((0x80 + (value & 0x3F)) as u8);
    }
}
