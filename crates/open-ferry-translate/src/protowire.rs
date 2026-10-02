// Ported from google.golang.org/protobuf encoding/protowire (v1.34.1, the
// version CLIProxyAPI uses; BSD-3-Clause, see licenses/protobuf-go-LICENSE).
// https://github.com/protocolbuffers/protobuf-go/blob/v1.34.1/encoding/protowire/wire.go

//! The low-level protobuf wire reader that upstream uses to inspect signature
//! envelopes. Only the `Consume*` functions are ported. They report the same
//! errors as protowire, which upstream includes in its messages.

use std::fmt;

/// A protobuf field number. As in protowire, it is negative when the tag's
/// number overflows `i32`.
pub(crate) type Number = i32;

/// A wire type. Values 6 and 7 are reserved.
pub(crate) type Type = u8;

pub(crate) const VARINT_TYPE: Type = 0;
pub(crate) const FIXED64_TYPE: Type = 1;
pub(crate) const BYTES_TYPE: Type = 2;
pub(crate) const START_GROUP_TYPE: Type = 3;
pub(crate) const END_GROUP_TYPE: Type = 4;
pub(crate) const FIXED32_TYPE: Type = 5;

/// How deep groups may nest, as in protowire's `DefaultRecursionLimit`.
const RECURSION_LIMIT: usize = 10_000;

/// protowire's parse errors, with the same messages as `ParseError`.
///
/// `ParseError` returns `io.ErrUnexpectedEOF` for truncation, and for the rest
/// its own errors, which start with `proto:` and a space. protobuf-go makes
/// that space a non-breaking one in some builds, picked per binary so that
/// callers don't compare error strings. We always use a regular space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Truncated,
    FieldNumber,
    Overflow,
    Reserved,
    EndGroup,
    RecursionDepth,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Truncated => "unexpected EOF",
            Error::FieldNumber => "proto: invalid field number",
            Error::Overflow => "proto: variable length integer overflow",
            Error::Reserved => "proto: cannot parse reserved wire type",
            Error::EndGroup => "proto: mismatching end group marker",
            Error::RecursionDepth => "proto: parse error",
        })
    }
}

/// `ConsumeVarint`: the value and its encoded length.
pub(crate) fn consume_varint(b: &[u8]) -> Result<(u64, usize), Error> {
    let mut value = 0u64;
    for i in 0..10 {
        let byte = *b.get(i).ok_or(Error::Truncated)?;
        if i == 9 {
            // The tenth byte holds only the top bit.
            return if byte < 2 {
                Ok((value | u64::from(byte) << 63, 10))
            } else {
                Err(Error::Overflow)
            };
        }
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte < 0x80 {
            return Ok((value, i + 1));
        }
    }
    unreachable!("the tenth byte always returns")
}

/// `ConsumeTag`: the field number, wire type and encoded length.
pub(crate) fn consume_tag(b: &[u8]) -> Result<(Number, Type, usize), Error> {
    let (tag, n) = consume_varint(b)?;
    let (num, typ) = decode_tag(tag);
    if num < 1 {
        return Err(Error::FieldNumber);
    }
    Ok((num, typ, n))
}

/// `DecodeTag`.
fn decode_tag(tag: u64) -> (Number, Type) {
    match i32::try_from(tag >> 3) {
        Ok(num) => (num, (tag & 7) as Type),
        Err(_) => (-1, 0),
    }
}

/// `ConsumeFixed32`.
pub(crate) fn consume_fixed32(b: &[u8]) -> Result<(u32, usize), Error> {
    let bytes = b.get(..4).ok_or(Error::Truncated)?;
    Ok((u32::from_le_bytes(bytes.try_into().expect("4 bytes")), 4))
}

/// `ConsumeFixed64`.
pub(crate) fn consume_fixed64(b: &[u8]) -> Result<(u64, usize), Error> {
    let bytes = b.get(..8).ok_or(Error::Truncated)?;
    Ok((u64::from_le_bytes(bytes.try_into().expect("8 bytes")), 8))
}

/// `ConsumeBytes`: a length-prefixed value and the encoded length.
pub(crate) fn consume_bytes(b: &[u8]) -> Result<(&[u8], usize), Error> {
    let (len, n) = consume_varint(b)?;
    let rest = &b[n..];
    let len = usize::try_from(len)
        .ok()
        .filter(|&len| len <= rest.len())
        .ok_or(Error::Truncated)?;
    Ok((&rest[..len], n + len))
}

/// `ConsumeFieldValue`: the length of a field's value, given its tag. A group's
/// length includes its end marker, whose number must match `num`.
pub(crate) fn consume_field_value(num: Number, typ: Type, b: &[u8]) -> Result<usize, Error> {
    match typ {
        VARINT_TYPE => consume_varint(b).map(|(_, n)| n),
        FIXED32_TYPE => consume_fixed32(b).map(|(_, n)| n),
        FIXED64_TYPE => consume_fixed64(b).map(|(_, n)| n),
        BYTES_TYPE => consume_bytes(b).map(|(_, n)| n),
        START_GROUP_TYPE => consume_group(num, b),
        END_GROUP_TYPE => Err(Error::EndGroup),
        _ => Err(Error::Reserved),
    }
}

/// The body and end marker of a group. protowire recurses into nested groups;
/// this keeps a stack of open groups instead, with the same depth limit.
fn consume_group(num: Number, b: &[u8]) -> Result<usize, Error> {
    let mut open = vec![num];
    let mut offset = 0;
    loop {
        let (num, typ, n) = consume_tag(&b[offset..])?;
        offset += n;
        match typ {
            END_GROUP_TYPE => {
                if open.pop() != Some(num) {
                    return Err(Error::EndGroup);
                }
                if open.is_empty() {
                    return Ok(offset);
                }
            }
            START_GROUP_TYPE => {
                if open.len() > RECURSION_LIMIT {
                    return Err(Error::RecursionDepth);
                }
                open.push(num);
            }
            _ => offset += consume_field_value(num, typ, &b[offset..])?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints() {
        assert_eq!(consume_varint(&[0x01]), Ok((1, 1)));
        assert_eq!(consume_varint(&[0xac, 0x02]), Ok((300, 2)));
        assert_eq!(consume_varint(&[0x80]), Err(Error::Truncated));
        let max = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert_eq!(consume_varint(&max), Ok((u64::MAX, 10)));
        let overflow = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        assert_eq!(consume_varint(&overflow), Err(Error::Overflow));
        // Non-minimal encodings are accepted.
        assert_eq!(consume_varint(&[0x81, 0x80, 0x00]), Ok((1, 3)));
    }

    #[test]
    fn tags() {
        assert_eq!(consume_tag(&[0x12]), Ok((2, BYTES_TYPE, 1)));
        assert_eq!(consume_tag(&[0x02]), Err(Error::FieldNumber));
        // A field number above i32::MAX.
        let huge = [0xf8, 0xff, 0xff, 0xff, 0xff, 0x7f];
        assert_eq!(consume_tag(&huge), Err(Error::FieldNumber));
    }

    #[test]
    fn field_values() {
        assert_eq!(
            consume_bytes(&[0x02, b'a', b'b', b'c']),
            Ok((&b"ab"[..], 3))
        );
        assert_eq!(consume_bytes(&[0x05, b'a']), Err(Error::Truncated));
        assert_eq!(consume_field_value(1, 6, &[]), Err(Error::Reserved));
        assert_eq!(
            consume_field_value(1, END_GROUP_TYPE, &[]),
            Err(Error::EndGroup)
        );
        // Group 1 holding varint field 2 and group 3, then its end marker.
        let group = [0x10, 0x05, 0x1b, 0x1c, 0x0c, 0xff];
        assert_eq!(consume_field_value(1, START_GROUP_TYPE, &group), Ok(5));
        assert_eq!(
            consume_field_value(2, START_GROUP_TYPE, &group),
            Err(Error::EndGroup)
        );
        assert_eq!(
            consume_field_value(1, START_GROUP_TYPE, &group[..2]),
            Err(Error::Truncated)
        );
    }

    #[test]
    fn group_depth_is_limited() {
        let nested = |depth: usize| {
            let mut b = vec![0x0b; depth];
            b.extend(std::iter::repeat_n(0x0c, depth + 1));
            b
        };
        assert!(consume_field_value(1, START_GROUP_TYPE, &nested(RECURSION_LIMIT)).is_ok());
        assert_eq!(
            consume_field_value(1, START_GROUP_TYPE, &nested(RECURSION_LIMIT + 1)),
            Err(Error::RecursionDepth)
        );
    }
}
